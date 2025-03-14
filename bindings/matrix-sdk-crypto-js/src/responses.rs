//! Types related to responses.

use std::borrow::Borrow;

use js_sys::{Array, JsString};
use matrix_sdk_common::deserialized_responses::{AlgorithmInfo, EncryptionInfo};
use matrix_sdk_crypto::IncomingResponse;
pub(crate) use ruma::api::client::{
    backup::add_backup_keys::v3::Response as KeysBackupResponse,
    keys::{
        claim_keys::v3::Response as KeysClaimResponse, get_keys::v3::Response as KeysQueryResponse,
        upload_keys::v3::Response as KeysUploadResponse,
        upload_signatures::v3::Response as SignatureUploadResponse,
    },
    message::send_message_event::v3::Response as RoomMessageResponse,
    to_device::send_event_to_device::v3::Response as ToDeviceResponse,
};
use ruma::api::IncomingResponse as RumaIncomingResponse;
use wasm_bindgen::prelude::*;

use crate::{encryption, identifiers, requests::RequestType};

pub(crate) fn response_from_string(body: &str) -> http::Result<http::Response<Vec<u8>>> {
    http::Response::builder().status(200).body(body.as_bytes().to_vec())
}

/// Intermediate private type to store an incoming owned response,
/// without the need to manage lifetime.
pub(crate) enum OwnedResponse {
    KeysUpload(KeysUploadResponse),
    KeysQuery(KeysQueryResponse),
    KeysClaim(KeysClaimResponse),
    ToDevice(ToDeviceResponse),
    SignatureUpload(SignatureUploadResponse),
    RoomMessage(RoomMessageResponse),
    KeysBackup(KeysBackupResponse),
}

impl From<KeysUploadResponse> for OwnedResponse {
    fn from(response: KeysUploadResponse) -> Self {
        OwnedResponse::KeysUpload(response)
    }
}

impl From<KeysQueryResponse> for OwnedResponse {
    fn from(response: KeysQueryResponse) -> Self {
        OwnedResponse::KeysQuery(response)
    }
}

impl From<KeysClaimResponse> for OwnedResponse {
    fn from(response: KeysClaimResponse) -> Self {
        OwnedResponse::KeysClaim(response)
    }
}

impl From<ToDeviceResponse> for OwnedResponse {
    fn from(response: ToDeviceResponse) -> Self {
        OwnedResponse::ToDevice(response)
    }
}

impl From<SignatureUploadResponse> for OwnedResponse {
    fn from(response: SignatureUploadResponse) -> Self {
        Self::SignatureUpload(response)
    }
}

impl From<RoomMessageResponse> for OwnedResponse {
    fn from(response: RoomMessageResponse) -> Self {
        OwnedResponse::RoomMessage(response)
    }
}

impl From<KeysBackupResponse> for OwnedResponse {
    fn from(r: KeysBackupResponse) -> Self {
        Self::KeysBackup(r)
    }
}

impl TryFrom<(RequestType, http::Response<Vec<u8>>)> for OwnedResponse {
    type Error = JsError;

    fn try_from(
        (request_type, response): (RequestType, http::Response<Vec<u8>>),
    ) -> Result<Self, Self::Error> {
        match request_type {
            RequestType::KeysUpload => {
                KeysUploadResponse::try_from_http_response(response).map(Into::into)
            }

            RequestType::KeysQuery => {
                KeysQueryResponse::try_from_http_response(response).map(Into::into)
            }

            RequestType::KeysClaim => {
                KeysClaimResponse::try_from_http_response(response).map(Into::into)
            }

            RequestType::ToDevice => {
                ToDeviceResponse::try_from_http_response(response).map(Into::into)
            }

            RequestType::SignatureUpload => {
                SignatureUploadResponse::try_from_http_response(response).map(Into::into)
            }

            RequestType::RoomMessage => {
                RoomMessageResponse::try_from_http_response(response).map(Into::into)
            }

            RequestType::KeysBackup => {
                KeysBackupResponse::try_from_http_response(response).map(Into::into)
            }
        }
        .map_err(JsError::from)
    }
}

impl<'a> From<&'a OwnedResponse> for IncomingResponse<'a> {
    fn from(response: &'a OwnedResponse) -> Self {
        match response {
            OwnedResponse::KeysUpload(response) => IncomingResponse::KeysUpload(response),
            OwnedResponse::KeysQuery(response) => IncomingResponse::KeysQuery(response),
            OwnedResponse::KeysClaim(response) => IncomingResponse::KeysClaim(response),
            OwnedResponse::ToDevice(response) => IncomingResponse::ToDevice(response),
            OwnedResponse::SignatureUpload(response) => IncomingResponse::SignatureUpload(response),
            OwnedResponse::RoomMessage(response) => IncomingResponse::RoomMessage(response),
            OwnedResponse::KeysBackup(response) => IncomingResponse::KeysBackup(response),
        }
    }
}

/// A decrypted room event.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug)]
pub struct DecryptedRoomEvent {
    /// The JSON-encoded decrypted event.
    #[wasm_bindgen(readonly)]
    pub event: JsString,

    encryption_info: Option<EncryptionInfo>,
}

#[wasm_bindgen]
impl DecryptedRoomEvent {
    /// The user ID of the event sender, note this is untrusted data
    /// unless the `verification_state` is as well trusted.
    #[wasm_bindgen(getter)]
    pub fn sender(&self) -> Option<identifiers::UserId> {
        Some(identifiers::UserId::from(self.encryption_info.as_ref()?.sender.clone()))
    }

    /// The device ID of the device that sent us the event, note this
    /// is untrusted data unless `verification_state` is as well
    /// trusted.
    #[wasm_bindgen(getter, js_name = "senderDevice")]
    pub fn sender_device(&self) -> Option<identifiers::DeviceId> {
        Some(identifiers::DeviceId::from(self.encryption_info.as_ref()?.sender_device.clone()))
    }

    /// The Curve25519 key of the device that created the megolm
    /// decryption key originally.
    #[wasm_bindgen(getter, js_name = "senderCurve25519Key")]
    pub fn sender_curve25519_key(&self) -> Option<JsString> {
        Some(match &self.encryption_info.as_ref()?.algorithm_info {
            AlgorithmInfo::MegolmV1AesSha2 { curve25519_key, .. } => curve25519_key.clone().into(),
        })
    }

    /// The signing Ed25519 key that have created the megolm key that
    /// was used to decrypt this session.
    #[wasm_bindgen(getter, js_name = "senderClaimedEd25519Key")]
    pub fn sender_claimed_ed25519_key(&self) -> Option<JsString> {
        match &self.encryption_info.as_ref()?.algorithm_info {
            AlgorithmInfo::MegolmV1AesSha2 { sender_claimed_keys, .. } => {
                sender_claimed_keys.get(&ruma::DeviceKeyAlgorithm::Ed25519).cloned().map(Into::into)
            }
        }
    }

    /// Chain of Curve25519 keys through which this session was
    /// forwarded, via `m.forwarded_room_key` events.
    #[wasm_bindgen(getter, js_name = "forwardingCurve25519KeyChain")]
    pub fn forwarding_curve25519_key_chain(&self) -> Array {
        Array::new()
    }

    /// The verification state of the device that sent us the event,
    /// note this is the state of the device at the time of
    /// decryption. It may change in the future if a device gets
    /// verified or deleted.
    #[wasm_bindgen(getter, js_name = "verificationState")]
    pub fn verification_state(&self) -> Option<encryption::VerificationState> {
        Some((self.encryption_info.as_ref()?.verification_state.borrow()).into())
    }
}

impl From<matrix_sdk_common::deserialized_responses::TimelineEvent> for DecryptedRoomEvent {
    fn from(value: matrix_sdk_common::deserialized_responses::TimelineEvent) -> Self {
        Self {
            event: value.event.json().get().to_owned().into(),
            encryption_info: value.encryption_info,
        }
    }
}
