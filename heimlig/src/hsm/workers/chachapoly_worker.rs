use crate::common::jobs::Error::NoKeyStore;
use crate::common::jobs::{ClientId, Error, Request, RequestId, Response};
use crate::config::keystore::MAX_KEY_SIZE;
use crate::crypto;
use crate::hsm::keystore::{KeyId, KeyStore};
use core::ops::DerefMut;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::mutex::Mutex;
use futures::{Sink, SinkExt, Stream, StreamExt};
use zeroize::Zeroizing;

pub struct ChaChaPolyWorker<
    'data,
    'keystore,
    M: RawMutex,
    ReqSrc: Stream<Item = Request<'data>>,
    RespSink: Sink<Response<'data>>,
> {
    pub key_store: Option<&'keystore Mutex<M, &'keystore mut (dyn KeyStore + Send)>>,
    pub requests: ReqSrc,
    pub responses: RespSink,
}

impl<
        'data,
        'keystore,
        M: RawMutex,
        ReqSrc: Stream<Item = Request<'data>> + Unpin,
        RespSink: Sink<Response<'data>> + Unpin,
    > ChaChaPolyWorker<'data, 'keystore, M, ReqSrc, RespSink>
{
    /// Drive the worker to process the next request.
    /// This method is supposed to be called by a system task that owns this worker.
    pub async fn execute(&mut self) -> Result<(), Error> {
        match self.requests.next().await {
            None => Ok(()), // Nothing to process
            Some(request) => {
                let response = match request {
                    Request::EncryptChaChaPoly {
                        client_id,
                        request_id,
                        key_id,
                        nonce,
                        plaintext,
                        aad,
                        tag,
                    } => self.encrypt_internal_key(
                        client_id, request_id, key_id, nonce, plaintext, aad, tag,
                    ),
                    Request::EncryptChaChaPolyExternalKey {
                        client_id,
                        request_id,
                        key,
                        nonce,
                        plaintext,
                        aad,
                        tag,
                    } => self.encrypt_with_external_key(
                        client_id, request_id, key, nonce, aad, plaintext, tag,
                    ),
                    Request::DecryptChaChaPoly {
                        client_id,
                        request_id,
                        key_id,
                        nonce,
                        ciphertext,
                        aad,
                        tag,
                    } => self.decrypt_with_internal_key(
                        client_id, request_id, key_id, nonce, ciphertext, aad, tag,
                    ),
                    Request::DecryptChaChaPolyExternalKey {
                        client_id,
                        request_id,
                        key,
                        nonce,
                        ciphertext,
                        aad,
                        tag,
                    } => self.decrypt_with_external_key(
                        client_id, request_id, key, nonce, aad, ciphertext, tag,
                    ),
                    _ => Err(Error::UnexpectedRequestType)?,
                };
                self.responses
                    .send(response)
                    .await
                    .map_err(|_e| Error::Send)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn encrypt_internal_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key_id: KeyId,
        nonce: &'data [u8],
        plaintext: &'data mut [u8],
        aad: &'data [u8],
        tag: &'data mut [u8],
    ) -> Response<'data> {
        match self.key_store {
            None => Response::Error {
                client_id,
                request_id,
                error: NoKeyStore,
            },
            Some(key_store) => {
                let mut key_buffer = Zeroizing::new([0u8; MAX_KEY_SIZE]);
                let export = key_store
                    .try_lock()
                    .expect("Failed to lock key store")
                    .deref_mut()
                    .export(key_id, key_buffer.as_mut_slice());
                match export {
                    Ok(key) => self.encrypt(client_id, request_id, key, nonce, aad, plaintext, tag),
                    Err(e) => Response::Error {
                        client_id,
                        request_id,
                        error: Error::KeyStore(e),
                    },
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decrypt_with_internal_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key_id: KeyId,
        nonce: &'data [u8],
        ciphertext: &'data mut [u8],
        aad: &'data [u8],
        tag: &'data [u8],
    ) -> Response<'data> {
        match self.key_store {
            None => Response::Error {
                client_id,
                request_id,
                error: NoKeyStore,
            },
            Some(key_store) => {
                let mut key_buffer = Zeroizing::new([0u8; MAX_KEY_SIZE]);
                let export = key_store
                    .try_lock()
                    .expect("Failed to lock key store")
                    .deref_mut()
                    .export(key_id, key_buffer.as_mut_slice());
                match export {
                    Ok(key) => {
                        self.decrypt(client_id, request_id, key, nonce, aad, ciphertext, tag)
                    }
                    Err(e) => Response::Error {
                        client_id,
                        request_id,
                        error: Error::KeyStore(e),
                    },
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn encrypt_with_external_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        nonce: &[u8],
        aad: &[u8],
        ciphertext: &'data mut [u8],
        tag: &'data mut [u8],
    ) -> Response<'data> {
        self.encrypt(client_id, request_id, key, nonce, aad, ciphertext, tag)
    }

    #[allow(clippy::too_many_arguments)]
    fn decrypt_with_external_key(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        nonce: &[u8],
        aad: &[u8],
        plaintext: &'data mut [u8],
        tag: &[u8],
    ) -> Response<'data> {
        self.decrypt(client_id, request_id, key, nonce, aad, plaintext, tag)
    }

    #[allow(clippy::too_many_arguments)]
    fn encrypt(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        nonce: &[u8],
        aad: &[u8],
        ciphertext: &'data mut [u8],
        tag: &'data mut [u8],
    ) -> Response<'data> {
        match crypto::chacha20poly1305::encrypt_in_place_detached(key, nonce, aad, ciphertext) {
            Ok(computed_tag) => {
                if computed_tag.len() != tag.len() {
                    return Response::Error {
                        client_id,
                        request_id,
                        error: Error::Crypto(crypto::Error::InvalidTagSize),
                    };
                }
                tag.copy_from_slice(computed_tag.as_slice());
                Response::EncryptChaChaPoly {
                    client_id,
                    request_id,
                    ciphertext,
                    tag,
                }
            }
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decrypt(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        key: &[u8],
        nonce: &[u8],
        aad: &[u8],
        plaintext: &'data mut [u8],
        tag: &[u8],
    ) -> Response<'data> {
        match crypto::chacha20poly1305::decrypt_in_place_detached(key, nonce, aad, plaintext, tag) {
            Ok(()) => Response::DecryptChaChaPoly {
                client_id,
                request_id,
                plaintext,
            },
            Err(e) => Response::Error {
                client_id,
                request_id,
                error: Error::Crypto(e),
            },
        }
    }
}
