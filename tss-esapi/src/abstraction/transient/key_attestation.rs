// Copyright 2021 Contributors to the Parsec project.
// SPDX-License-Identifier: Apache-2.0
use super::{ObjectWrapper, TransientKeyContext};
use crate::{
    abstraction::ek,
    constants::SessionType,
    handles::{AuthHandle, KeyHandle, SessionHandle},
    interface_types::{
        algorithm::{AsymmetricAlgorithm, HashingAlgorithm},
        session_handles::{AuthSession, PolicySession},
    },
    structures::{EncryptedSecret, IDObject, SymmetricDefinition},
    tss2_esys::{Tss2_MU_TPM2B_PUBLIC_Marshal, TPM2B_PUBLIC},
    utils::PublicKey,
    Error, Result,
};
use log::error;
use std::convert::TryFrom;

#[derive(Debug)]
/// Wrapper for the parameters needed by MakeCredential
pub struct MakeCredParams {
    /// TPM name of the object
    pub name: Vec<u8>,
    /// Encoding of the public parameters of the object whose name
    /// will be included in the credential computations
    pub public: Vec<u8>,
    /// Public part of the key used to protect the credential
    pub attesting_key_pub: PublicKey,
}

impl TransientKeyContext {
    /// Get the data required to perform a MakeCredential
    ///
    /// # Parameters
    ///
    /// * `object` - the object whose TPM name will be included in
    /// the credential
    /// * `key` - the key to be used to encrypt the secret that wraps
    /// the credential
    ///
    /// **Note**: If no `key` is given, the default Endorsement Key
    /// will be used.  
    pub fn get_make_cred_params(
        &mut self,
        object: ObjectWrapper,
        key: Option<ObjectWrapper>,
    ) -> Result<MakeCredParams> {
        let object_handle = self.load_key(object.params, object.material, None)?;
        let (object_public, object_name, _) =
            self.context.read_public(object_handle).or_else(|e| {
                self.context.flush_context(object_handle.into())?;
                Err(e)
            })?;
        self.context.flush_context(object_handle.into())?;

        let public = TPM2B_PUBLIC::from(object_public);
        let mut pub_buf = [0u8; std::mem::size_of::<TPM2B_PUBLIC>()];
        let mut offset = 0;
        let result = unsafe {
            Tss2_MU_TPM2B_PUBLIC_Marshal(
                &public,
                &mut pub_buf as *mut u8,
                pub_buf.len() as u64,
                &mut offset,
            )
        };
        let result = Error::from_tss_rc(result);
        if !result.is_success() {
            error!("Error in marshalling TPM2B");
            return Err(result);
        }

        let attesting_key_pub = match key {
            None => get_ek_object_public(&mut self.context)?,
            Some(key) => key.material.public,
        };
        Ok(MakeCredParams {
            name: object_name.value().to_vec(),
            public: pub_buf.to_vec(),
            attesting_key_pub,
        })
    }

    /// Perform an ActivateCredential operation for the given object
    ///
    /// # Parameters
    ///
    /// * `object` - the object whose TPM name is included in the credential
    /// * `key` - the key used to encrypt the secret that wraps the credential
    /// * `credential_blob` - encrypted credential that will be returned by the
    /// TPM
    /// * `secret` - encrypted secret that was used to encrypt the credential
    ///
    /// **Note**: if no `key` is given, the default Endorsement Key
    /// will be used. You can find more information about the default Endorsement
    /// Key in the [ek] module.
    pub fn activate_credential(
        &mut self,
        object: ObjectWrapper,
        key: Option<ObjectWrapper>,
        credential_blob: Vec<u8>,
        secret: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let credential_blob = IDObject::try_from(credential_blob)?;
        let secret = EncryptedSecret::try_from(secret)?;
        let object_handle = self.load_key(object.params, object.material, object.auth)?;
        let (key_handle, session_2) = match key {
            Some(key) => self.prepare_key_activate_cred(key),
            None => self.prepare_ek_activate_cred(),
        }
        .or_else(|e| {
            self.context.flush_context(object_handle.into())?;
            Err(e)
        })?;

        let (session_1, _, _) = self.context.sessions();
        let credential = self
            .context
            .execute_with_sessions((session_1, session_2, None), |ctx| {
                ctx.activate_credential(object_handle, key_handle, credential_blob, secret)
            })
            .or_else(|e| {
                self.context.flush_context(object_handle.into())?;
                self.context.flush_context(key_handle.into())?;
                self.context
                    .flush_context(SessionHandle::from(session_2).into())?;
                Err(e)
            })?;

        self.context.flush_context(object_handle.into())?;
        self.context.flush_context(key_handle.into())?;
        self.context
            .flush_context(SessionHandle::from(session_2).into())?;
        Ok(credential.value().to_vec())
    }

    // No key was given, use the EK. This requires using a Policy session
    fn prepare_ek_activate_cred(&mut self) -> Result<(KeyHandle, Option<AuthSession>)> {
        let session = self.context.start_auth_session(
            None,
            None,
            None,
            SessionType::Policy,
            SymmetricDefinition::AES_128_CFB,
            HashingAlgorithm::Sha256,
        )?;
        let _ = self.context.policy_secret(
            PolicySession::try_from(session.unwrap())
                .expect("Failed to convert auth session to policy session"),
            AuthHandle::Endorsement,
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        );
        Ok((
            ek::create_ek_object(&mut self.context, AsymmetricAlgorithm::Rsa, None).or_else(
                |e| {
                    self.context
                        .flush_context(SessionHandle::from(session).into())?;
                    Err(e)
                },
            )?,
            session,
        ))
    }

    // Load key and create a HMAC session for it
    fn prepare_key_activate_cred(
        &mut self,
        key: ObjectWrapper,
    ) -> Result<(KeyHandle, Option<AuthSession>)> {
        let session = self.context.start_auth_session(
            None,
            None,
            None,
            SessionType::Hmac,
            SymmetricDefinition::AES_128_CFB,
            HashingAlgorithm::Sha256,
        )?;
        Ok((
            self.load_key(key.params, key.material, key.auth)
                .or_else(|e| {
                    self.context
                        .flush_context(SessionHandle::from(session).into())?;
                    Err(e)
                })?,
            session,
        ))
    }
}

fn get_ek_object_public(context: &mut crate::Context) -> Result<PublicKey> {
    let key_handle = ek::create_ek_object(context, AsymmetricAlgorithm::Rsa, None)?;
    let (attesting_key_pub, _, _) = context.read_public(key_handle).or_else(|e| {
        context.flush_context(key_handle.into())?;
        Err(e)
    })?;
    context.flush_context(key_handle.into())?;

    PublicKey::try_from(attesting_key_pub)
}
