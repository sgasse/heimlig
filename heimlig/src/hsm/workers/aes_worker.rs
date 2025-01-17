use crate::common::jobs::{ClientId, Error, Request, RequestId, Response};
use crate::crypto;
use crate::crypto::aes::cbc::{
    aes128cbc_decrypt, aes128cbc_encrypt, aes192cbc_decrypt, aes192cbc_encrypt, aes256cbc_decrypt,
    aes256cbc_encrypt,
};
use crate::crypto::aes::gcm::{
    aes128gcm_decrypt_in_place_detached, aes128gcm_encrypt_in_place_detached,
    aes256gcm_decrypt_in_place_detached, aes256gcm_encrypt_in_place_detached,
};
use crate::crypto::aes::{KEY128_SIZE, KEY192_SIZE, KEY256_SIZE};
use crate::hsm::keystore;
use crate::hsm::keystore::{KeyId, KeyInfo, KeyStore, KeyType};
use cbc::cipher::block_padding::Pkcs7;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::mutex::Mutex;
use futures::{Sink, SinkExt, Stream, StreamExt};
use zeroize::Zeroizing;

pub struct AesWorker<
    'data,
    'keystore,
    M: RawMutex,
    ReqSrc: Stream<Item = Request<'data>>,
    RespSink: Sink<Response<'data>>,
> {
    pub key_store: &'keystore Mutex<M, &'keystore mut (dyn KeyStore + Send)>,
    pub requests: ReqSrc,
    pub responses: RespSink,
}

impl<
        'data,
        'rng,
        'keystore,
        M: RawMutex,
        ReqSrc: Stream<Item = Request<'data>> + Unpin,
        RespSink: Sink<Response<'data>> + Unpin,
    > AesWorker<'data, 'keystore, M, ReqSrc, RespSink>
{
    /// Drive the worker to process the next request.
    /// This method is supposed to be called by a system task that owns this worker.
    pub async fn execute(&mut self) -> Result<(), Error> {
        let request = self.requests.next().await.ok_or(Error::StreamTerminated)?;
        let response = match request {
            Request::EncryptAesGcm {
                client_id,
                request_id,
                key_id,
                iv,
                buffer,
                aad,
                tag,
            } => {
                self.encrypt_aes_gcm(client_id, request_id, key_id, iv, buffer, aad, tag)
                    .await
            }
            Request::EncryptAesGcmExternalKey {
                client_id,
                request_id,
                key,
                iv,
                buffer,
                aad,
                tag,
            } => {
                self.encrypt_aes_gcm_external_key(client_id, request_id, key, iv, buffer, aad, tag)
                    .await
            }
            Request::DecryptAesGcm {
                client_id,
                request_id,
                key_id,
                iv,
                buffer,
                aad,
                tag,
            } => {
                self.decrypt_aes_gcm(client_id, request_id, key_id, iv, buffer, aad, tag)
                    .await
            }
            Request::DecryptAesGcmExternalKey {
                client_id,
                request_id,
                key,
                iv,
                buffer,
                aad,
                tag,
            } => {
                self.decrypt_aes_gcm_external_key(client_id, request_id, key, iv, buffer, aad, tag)
                    .await
            }
            Request::EncryptAesCbc {
                client_id,
                request_id,
                key_id,
                iv,
                buffer,
                plaintext_size,
            } => {
                self.encrypt_aes_cbc(client_id, request_id, key_id, iv, buffer, plaintext_size)
                    .await
            }
            Request::EncryptAesCbcExternalKey {
                client_id,
                request_id,
                key,
                iv,
                buffer,
                plaintext_size,
            } => {
                self.encrypt_aes_cbc_external_key(
                    client_id,
                    request_id,
                    key,
                    iv,
                    buffer,
                    plaintext_size,
                )
                .await
            }
            Request::DecryptAesCbc {
                client_id,
                request_id,
                key_id,
                iv,
                buffer,
            } => {
                self.decrypt_aes_cbc(client_id, request_id, key_id, iv, buffer)
                    .await
            }
            Request::DecryptAesCbcExternalKey {
                client_id,
                request_id,
                key,
                iv,
                buffer,
            } => {
                self.decrypt_aes_cbc_external_key(client_id, request_id, key, iv, buffer)
                    .await
            }
            _ => Err(Error::UnexpectedRequestType)?,
        };
        self.responses
            .send(response)
            .await
            .map_err(|_e| Error::Send)
    }

    #[allow(clippy::too_many_arguments)]
    async fn encrypt_aes_gcm(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key_id: KeyId,
        iv: &[u8],
        buffer: &'data mut [u8],
        aad: &[u8],
        tag: &'data mut [u8],
    ) -> Response<'data> {
        let mut key_buffer = Zeroizing::new([0u8; KeyType::MAX_SYMMETRIC_KEY_SIZE]);
        let key_and_info = self
            .export_key_and_key_info(key_id, key_buffer.as_mut_slice())
            .await;
        let result = match key_and_info {
            Err(e) => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::KeyStore(e),
                }
            }
            Ok((key, key_info)) => match key_info.ty {
                KeyType::Symmetric128Bits => {
                    aes128gcm_encrypt_in_place_detached(key, iv, aad, buffer, tag)
                }
                KeyType::Symmetric256Bits => {
                    aes256gcm_encrypt_in_place_detached(key, iv, aad, buffer, tag)
                }
                _ => {
                    return Response::Error {
                        client_id,
                        request_id,
                        error: Error::KeyStore(keystore::Error::InvalidKeyType),
                    }
                }
            },
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(()) => Response::EncryptAesGcm {
                client_id,
                request_id,
                buffer,
                tag,
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn encrypt_aes_gcm_external_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        iv: &[u8],
        buffer: &'data mut [u8],
        aad: &[u8],
        tag: &'data mut [u8],
    ) -> Response<'data> {
        let result = match key.len() {
            KEY128_SIZE => aes128gcm_encrypt_in_place_detached(key, iv, aad, buffer, tag),
            KEY256_SIZE => aes256gcm_encrypt_in_place_detached(key, iv, aad, buffer, tag),
            _ => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::Crypto(crypto::Error::InvalidSymmetricKeySize),
                }
            }
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(()) => Response::EncryptAesGcm {
                client_id,
                request_id,
                buffer,
                tag,
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn decrypt_aes_gcm(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key_id: KeyId,
        iv: &[u8],
        buffer: &'data mut [u8],
        aad: &[u8],
        tag: &[u8],
    ) -> Response<'data> {
        let mut key_buffer = Zeroizing::new([0u8; KeyType::MAX_SYMMETRIC_KEY_SIZE]);
        let key_and_info = self
            .export_key_and_key_info(key_id, key_buffer.as_mut_slice())
            .await;
        let result = match key_and_info {
            Err(e) => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::KeyStore(e),
                }
            }
            Ok((key, key_info)) => match key_info.ty {
                KeyType::Symmetric128Bits => {
                    aes128gcm_decrypt_in_place_detached(key, iv, aad, buffer, tag)
                }
                KeyType::Symmetric256Bits => {
                    aes256gcm_decrypt_in_place_detached(key, iv, aad, buffer, tag)
                }
                _ => {
                    return Response::Error {
                        client_id,
                        request_id,
                        error: Error::KeyStore(keystore::Error::InvalidKeyType),
                    }
                }
            },
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(()) => Response::DecryptAesGcm {
                client_id,
                request_id,
                buffer,
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn decrypt_aes_gcm_external_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        iv: &[u8],
        buffer: &'data mut [u8],
        aad: &[u8],
        tag: &[u8],
    ) -> Response<'data> {
        let result = match key.len() {
            KEY128_SIZE => aes128gcm_decrypt_in_place_detached(key, iv, aad, buffer, tag),
            KEY256_SIZE => aes256gcm_decrypt_in_place_detached(key, iv, aad, buffer, tag),
            _ => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::Crypto(crypto::Error::InvalidSymmetricKeySize),
                }
            }
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(()) => Response::DecryptAesGcm {
                client_id,
                request_id,
                buffer,
            },
        }
    }

    async fn encrypt_aes_cbc(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key_id: KeyId,
        iv: &[u8],
        buffer: &'data mut [u8],
        plaintext_size: usize,
    ) -> Response<'data> {
        let mut key_buffer = Zeroizing::new([0u8; KeyType::MAX_SYMMETRIC_KEY_SIZE]);
        let key_and_info = self
            .export_key_and_key_info(key_id, key_buffer.as_mut_slice())
            .await;
        let result = match key_and_info {
            Err(e) => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::KeyStore(e),
                }
            }
            Ok((key, key_info)) => match key_info.ty {
                KeyType::Symmetric128Bits => {
                    aes128cbc_encrypt::<Pkcs7>(key, iv, buffer, plaintext_size)
                }
                KeyType::Symmetric192Bits => {
                    aes192cbc_encrypt::<Pkcs7>(key, iv, buffer, plaintext_size)
                }
                KeyType::Symmetric256Bits => {
                    aes256cbc_encrypt::<Pkcs7>(key, iv, buffer, plaintext_size)
                }
                _ => {
                    return Response::Error {
                        client_id,
                        request_id,
                        error: Error::KeyStore(keystore::Error::InvalidKeyType),
                    }
                }
            },
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(ciphertext) => {
                let ciphertext_len = ciphertext.len();
                Response::EncryptAesCbc {
                    client_id,
                    request_id,
                    buffer: &mut buffer[..ciphertext_len],
                }
            }
        }
    }

    async fn encrypt_aes_cbc_external_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        iv: &[u8],
        buffer: &'data mut [u8],
        plaintext_size: usize,
    ) -> Response<'data> {
        let result = match key.len() {
            KEY128_SIZE => aes128cbc_encrypt::<Pkcs7>(key, iv, buffer, plaintext_size),
            KEY192_SIZE => aes192cbc_encrypt::<Pkcs7>(key, iv, buffer, plaintext_size),
            KEY256_SIZE => aes256cbc_encrypt::<Pkcs7>(key, iv, buffer, plaintext_size),
            _ => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::Crypto(crypto::Error::InvalidSymmetricKeySize),
                }
            }
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(ciphertext) => {
                let ciphertext_len = ciphertext.len();
                Response::EncryptAesCbc {
                    client_id,
                    request_id,
                    buffer: &mut buffer[..ciphertext_len],
                }
            }
        }
    }

    async fn decrypt_aes_cbc(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key_id: KeyId,
        iv: &[u8],
        buffer: &'data mut [u8],
    ) -> Response<'data> {
        let mut key_buffer = Zeroizing::new([0u8; KeyType::MAX_SYMMETRIC_KEY_SIZE]);
        let key_and_info = self
            .export_key_and_key_info(key_id, key_buffer.as_mut_slice())
            .await;
        let result = match key_and_info {
            Err(e) => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::KeyStore(e),
                }
            }
            Ok((key, key_info)) => match key_info.ty {
                KeyType::Symmetric128Bits => aes128cbc_decrypt::<Pkcs7>(key, iv, buffer),
                KeyType::Symmetric192Bits => aes192cbc_decrypt::<Pkcs7>(key, iv, buffer),
                KeyType::Symmetric256Bits => aes256cbc_decrypt::<Pkcs7>(key, iv, buffer),
                _ => {
                    return Response::Error {
                        client_id,
                        request_id,
                        error: Error::KeyStore(keystore::Error::InvalidKeyType),
                    }
                }
            },
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(plaintext) => {
                let plaintext_len = plaintext.len();
                Response::DecryptAesCbc {
                    client_id,
                    request_id,
                    plaintext: &mut buffer[..plaintext_len],
                }
            }
        }
    }

    async fn decrypt_aes_cbc_external_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        iv: &[u8],
        buffer: &'data mut [u8],
    ) -> Response<'data> {
        let result = match key.len() {
            KEY128_SIZE => aes128cbc_decrypt::<Pkcs7>(key, iv, buffer),
            KEY192_SIZE => aes192cbc_decrypt::<Pkcs7>(key, iv, buffer),
            KEY256_SIZE => aes256cbc_decrypt::<Pkcs7>(key, iv, buffer),
            _ => {
                return Response::Error {
                    client_id,
                    request_id,
                    error: Error::Crypto(crypto::Error::InvalidSymmetricKeySize),
                }
            }
        };
        match result {
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
            Ok(plaintext) => {
                let plaintext_len = plaintext.len();
                Response::DecryptAesCbc {
                    client_id,
                    request_id,
                    plaintext: &mut buffer[..plaintext_len],
                }
            }
        }
    }

    async fn export_key_and_key_info<'a>(
        &mut self,
        key_id: KeyId,
        key_buffer: &'a mut [u8],
    ) -> Result<(&'a [u8], KeyInfo), keystore::Error> {
        // Lock keystore only once
        let locked_key_store = self.key_store.lock().await;
        Ok((
            locked_key_store.export_symmetric_key_unchecked(key_id, key_buffer)?,
            locked_key_store.get_key_info(key_id)?,
        ))
    }
}
