// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    matches,
    sync::{Arc, Mutex},
    time::Duration,
};

use matrix_sdk_common::instant::Instant;
use ruma::{
    events::{
        key::verification::{
            accept::{
                AcceptMethod, KeyVerificationAcceptEventContent, SasV1Content as AcceptV1Content,
                SasV1ContentInit as AcceptV1ContentInit, ToDeviceKeyVerificationAcceptEventContent,
            },
            cancel::CancelCode,
            done::{KeyVerificationDoneEventContent, ToDeviceKeyVerificationDoneEventContent},
            key::{KeyVerificationKeyEventContent, ToDeviceKeyVerificationKeyEventContent},
            start::{
                KeyVerificationStartEventContent, SasV1Content, SasV1ContentInit, StartMethod,
                ToDeviceKeyVerificationStartEventContent,
            },
            HashAlgorithm, KeyAgreementProtocol, MessageAuthenticationCode, Relation,
            ShortAuthenticationString, VerificationMethod,
        },
        AnyMessageLikeEventContent, AnyToDeviceEventContent,
    },
    serde::Base64,
    DeviceId, UserId,
};
use tracing::info;
use vodozemac::{
    sas::{EstablishedSas, Sas},
    Curve25519PublicKey,
};

use super::{
    helpers::{
        calculate_commitment, get_decimal, get_emoji, get_emoji_index, get_mac_content,
        receive_mac_event, SasIds,
    },
    OutgoingContent,
};
use crate::{
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    verification::{
        event_enums::{
            AcceptContent, DoneContent, KeyContent, MacContent, OwnedAcceptContent,
            OwnedStartContent, StartContent,
        },
        Cancelled, Emoji, FlowId,
    },
    ReadOnlyAccount, ReadOnlyOwnUserIdentity,
};

const KEY_AGREEMENT_PROTOCOLS: &[KeyAgreementProtocol] =
    &[KeyAgreementProtocol::Curve25519HkdfSha256];
const HASHES: &[HashAlgorithm] = &[HashAlgorithm::Sha256];
const MACS: &[MessageAuthenticationCode] = &[MessageAuthenticationCode::HkdfHmacSha256];
const STRINGS: &[ShortAuthenticationString] =
    &[ShortAuthenticationString::Decimal, ShortAuthenticationString::Emoji];

fn the_protocol_definitions() -> SasV1Content {
    SasV1ContentInit {
        short_authentication_string: STRINGS.to_vec(),
        key_agreement_protocols: KEY_AGREEMENT_PROTOCOLS.to_vec(),
        message_authentication_codes: MACS.to_vec(),
        hashes: HASHES.to_vec(),
    }
    .try_into()
    .expect("Invalid protocol definition.")
}

// The max time a SAS flow can take from start to done.
const MAX_AGE: Duration = Duration::from_secs(60 * 5);

// The max time a SAS object will wait for a new event to arrive.
const MAX_EVENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Struct containing the protocols that were agreed to be used for the SAS
/// flow.
#[derive(Clone, Debug)]
pub struct AcceptedProtocols {
    pub method: VerificationMethod,
    pub key_agreement_protocol: KeyAgreementProtocol,
    pub hash: HashAlgorithm,
    pub message_auth_code: MessageAuthenticationCode,
    pub short_auth_string: Vec<ShortAuthenticationString>,
}

impl TryFrom<AcceptV1Content> for AcceptedProtocols {
    type Error = CancelCode;

    fn try_from(content: AcceptV1Content) -> Result<Self, Self::Error> {
        if !KEY_AGREEMENT_PROTOCOLS.contains(&content.key_agreement_protocol)
            || !HASHES.contains(&content.hash)
            || !MACS.contains(&content.message_authentication_code)
            || (!content.short_authentication_string.contains(&ShortAuthenticationString::Emoji)
                && !content
                    .short_authentication_string
                    .contains(&ShortAuthenticationString::Decimal))
        {
            Err(CancelCode::UnknownMethod)
        } else {
            Ok(Self {
                method: VerificationMethod::SasV1,
                hash: content.hash,
                key_agreement_protocol: content.key_agreement_protocol,
                message_auth_code: content.message_authentication_code,
                short_auth_string: content.short_authentication_string,
            })
        }
    }
}

impl TryFrom<&SasV1Content> for AcceptedProtocols {
    type Error = CancelCode;

    fn try_from(method_content: &SasV1Content) -> Result<Self, Self::Error> {
        if !method_content
            .key_agreement_protocols
            .contains(&KeyAgreementProtocol::Curve25519HkdfSha256)
            || !method_content
                .message_authentication_codes
                .contains(&MessageAuthenticationCode::HkdfHmacSha256)
            || !method_content.hashes.contains(&HashAlgorithm::Sha256)
            || (!method_content
                .short_authentication_string
                .contains(&ShortAuthenticationString::Decimal)
                && !method_content
                    .short_authentication_string
                    .contains(&ShortAuthenticationString::Emoji))
        {
            Err(CancelCode::UnknownMethod)
        } else {
            let mut short_auth_string = vec![];

            if method_content
                .short_authentication_string
                .contains(&ShortAuthenticationString::Decimal)
            {
                short_auth_string.push(ShortAuthenticationString::Decimal)
            }

            if method_content
                .short_authentication_string
                .contains(&ShortAuthenticationString::Emoji)
            {
                short_auth_string.push(ShortAuthenticationString::Emoji);
            }

            Ok(Self {
                method: VerificationMethod::SasV1,
                hash: HashAlgorithm::Sha256,
                key_agreement_protocol: KeyAgreementProtocol::Curve25519HkdfSha256,
                message_auth_code: MessageAuthenticationCode::HkdfHmacSha256,
                short_auth_string,
            })
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl Default for AcceptedProtocols {
    fn default() -> Self {
        AcceptedProtocols {
            method: VerificationMethod::SasV1,
            hash: HashAlgorithm::Sha256,
            key_agreement_protocol: KeyAgreementProtocol::Curve25519HkdfSha256,
            message_auth_code: MessageAuthenticationCode::HkdfHmacSha256,
            short_auth_string: vec![
                ShortAuthenticationString::Decimal,
                ShortAuthenticationString::Emoji,
            ],
        }
    }
}

/// A type level state machine modeling the Sas flow.
///
/// This is the generic struct holding common data between the different states
/// and the specific state.
#[derive(Clone)]
pub struct SasState<S: Clone> {
    /// The SAS struct.
    inner: Arc<Mutex<Option<Sas>>>,

    /// The public key we generated for this SAS flow.
    our_public_key: Curve25519PublicKey,

    /// Struct holding the identities that are doing the SAS dance.
    ids: SasIds,

    /// The instant when the SAS object was created. If this more than
    /// MAX_AGE seconds are elapsed, the event will be canceled with a
    /// `CancelCode::Timeout`
    creation_time: Arc<Instant>,

    /// The instant the SAS object last received an event.
    last_event_time: Arc<Instant>,

    /// The unique identifier of this SAS flow.
    ///
    /// This will be the transaction id for to-device events and the relates_to
    /// field for in-room events.
    pub verification_flow_id: Arc<FlowId>,

    /// The SAS state we're in.
    pub state: Arc<S>,

    /// Did the SAS verification start from a `m.verification.request`.
    pub started_from_request: bool,
}

#[cfg(not(tarpaulin_include))]
impl<S: Clone + std::fmt::Debug> std::fmt::Debug for SasState<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SasState")
            .field("ids", &self.ids)
            .field("flow_id", &self.verification_flow_id)
            .field("state", &self.state)
            .finish()
    }
}

/// The initial SAS state.
#[derive(Clone, Debug)]
pub struct Created {
    protocol_definitions: SasV1Content,
}

/// The initial SAS state if the other side started the SAS verification.
#[derive(Clone, Debug)]
pub struct Started {
    commitment: Base64,
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after the other side accepted our
/// verification start event.
#[derive(Clone, Debug)]
pub struct Accepted {
    pub accepted_protocols: Arc<AcceptedProtocols>,
    start_content: Arc<OwnedStartContent>,
    commitment: Base64,
}

/// The SAS state we're going to be in after we accepted our
/// verification start event.
#[derive(Clone, Debug)]
pub struct WeAccepted {
    we_started: bool,
    pub accepted_protocols: Arc<AcceptedProtocols>,
    commitment: Base64,
}

/// The SAS state we're going to be in after we received the public key of the
/// other participant.
///
/// From now on we can show the short auth string to the user.
#[derive(Clone, Debug)]
pub struct KeyReceived {
    sas: Arc<Mutex<EstablishedSas>>,
    we_started: bool,
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after the user has confirmed that the
/// short auth string matches. We still need to receive a MAC event from the
/// other side.
#[derive(Clone, Debug)]
pub struct Confirmed {
    sas: Arc<Mutex<EstablishedSas>>,
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after we receive a MAC event from the
/// other side. Our own user still needs to confirm that the short auth string
/// matches.
#[derive(Clone, Debug)]
pub struct MacReceived {
    sas: Arc<Mutex<EstablishedSas>>,
    we_started: bool,
    verified_devices: Arc<[ReadOnlyDevice]>,
    verified_master_keys: Arc<[ReadOnlyUserIdentities]>,
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after we receive a MAC event in a DM. DMs
/// require a final message `m.key.verification.done` message to conclude the
/// verification. This state waits for such a message.
#[derive(Clone, Debug)]
pub struct WaitingForDone {
    sas: Arc<Mutex<EstablishedSas>>,
    verified_devices: Arc<[ReadOnlyDevice]>,
    verified_master_keys: Arc<[ReadOnlyUserIdentities]>,
}

/// The verification state indicating that the verification finished
/// successfully.
///
/// We can now mark the device in our verified devices list as verified and sign
/// the master keys in the verified devices list.
#[derive(Clone, Debug)]
pub struct Done {
    sas: Arc<Mutex<EstablishedSas>>,
    verified_devices: Arc<[ReadOnlyDevice]>,
    verified_master_keys: Arc<[ReadOnlyUserIdentities]>,
}

impl<S: Clone> SasState<S> {
    /// Get our own user id.
    #[cfg(test)]
    pub fn user_id(&self) -> &UserId {
        self.ids.account.user_id()
    }

    /// Get our own device ID.
    pub fn device_id(&self) -> &DeviceId {
        self.ids.account.device_id()
    }

    #[cfg(test)]
    pub fn other_device(&self) -> ReadOnlyDevice {
        self.ids.other_device.clone()
    }

    pub fn cancel(self, cancelled_by_us: bool, cancel_code: CancelCode) -> SasState<Cancelled> {
        SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            ids: self.ids,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            verification_flow_id: self.verification_flow_id,
            state: Arc::new(Cancelled::new(cancelled_by_us, cancel_code)),
            started_from_request: self.started_from_request,
        }
    }

    /// Did our SAS verification time out.
    pub fn timed_out(&self) -> bool {
        self.creation_time.elapsed() > MAX_AGE || self.last_event_time.elapsed() > MAX_EVENT_TIMEOUT
    }

    /// Is this verification happening inside a DM.
    #[allow(dead_code)]
    pub fn is_dm_verification(&self) -> bool {
        matches!(&*self.verification_flow_id, FlowId::InRoom(_, _))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn set_creation_time(&mut self, time: Instant) {
        self.creation_time = Arc::new(time);
    }

    fn check_event(&self, sender: &UserId, flow_id: &str) -> Result<(), CancelCode> {
        if *flow_id != *self.verification_flow_id.as_str() {
            Err(CancelCode::UnknownTransaction)
        } else if sender != self.ids.other_device.user_id() {
            Err(CancelCode::UserMismatch)
        } else if self.timed_out() {
            Err(CancelCode::Timeout)
        } else {
            Ok(())
        }
    }
}

impl SasState<Created> {
    /// Create a new SAS verification flow.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// * `other_identity` - The identity of the other user if one exists.
    pub fn new(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        flow_id: FlowId,
        started_from_request: bool,
    ) -> SasState<Created> {
        Self::new_helper(
            flow_id,
            account,
            other_device,
            own_identity,
            other_identity,
            started_from_request,
        )
    }

    fn new_helper(
        flow_id: FlowId,
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        started_from_request: bool,
    ) -> SasState<Created> {
        let sas = Sas::new();
        let our_public_key = sas.public_key();

        SasState {
            inner: Arc::new(Mutex::new(Some(sas))),
            our_public_key,
            ids: SasIds { account, other_device, other_identity, own_identity },
            verification_flow_id: flow_id.into(),

            creation_time: Arc::new(Instant::now()),
            last_event_time: Arc::new(Instant::now()),
            started_from_request,

            state: Arc::new(Created { protocol_definitions: the_protocol_definitions() }),
        }
    }

    pub fn as_content(&self) -> OwnedStartContent {
        match self.verification_flow_id.as_ref() {
            FlowId::ToDevice(s) => {
                OwnedStartContent::ToDevice(ToDeviceKeyVerificationStartEventContent::new(
                    self.device_id().into(),
                    s.clone(),
                    StartMethod::SasV1(self.state.protocol_definitions.clone()),
                ))
            }
            FlowId::InRoom(r, e) => OwnedStartContent::Room(
                r.clone(),
                KeyVerificationStartEventContent::new(
                    self.device_id().into(),
                    StartMethod::SasV1(self.state.protocol_definitions.clone()),
                    Relation::new(e.clone()),
                ),
            ),
        }
    }

    /// Receive a m.key.verification.accept event, changing the state into
    /// an Accepted one.
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.accept event that was sent to us by
    /// the other side.
    pub fn into_accepted(
        self,
        sender: &UserId,
        content: &AcceptContent<'_>,
    ) -> Result<SasState<Accepted>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        if let AcceptMethod::SasV1(content) = content.method() {
            let accepted_protocols = AcceptedProtocols::try_from(content.clone())
                .map_err(|c| self.clone().cancel(true, c))?;

            let start_content = self.as_content().into();

            Ok(SasState {
                inner: self.inner,
                our_public_key: self.our_public_key,
                ids: self.ids,
                verification_flow_id: self.verification_flow_id,
                creation_time: self.creation_time,
                last_event_time: Instant::now().into(),
                started_from_request: self.started_from_request,
                state: Arc::new(Accepted {
                    start_content,
                    commitment: content.commitment.clone(),
                    accepted_protocols: accepted_protocols.into(),
                }),
            })
        } else {
            Err(self.cancel(true, CancelCode::UnknownMethod))
        }
    }
}

impl SasState<Started> {
    /// Create a new SAS verification flow from an in-room
    /// m.key.verification.start event.
    ///
    /// This will put us in the `started` state.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// * `event` - The m.key.verification.start event that was sent to us by
    /// the other side.
    pub fn from_start_event(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        flow_id: FlowId,
        content: &StartContent<'_>,
        started_from_request: bool,
    ) -> Result<SasState<Started>, SasState<Cancelled>> {
        let flow_id = Arc::new(flow_id);

        let sas = Sas::new();
        let our_public_key = sas.public_key();

        let canceled = || SasState {
            inner: Arc::new(Mutex::new(None)),
            our_public_key,

            creation_time: Arc::new(Instant::now()),
            last_event_time: Arc::new(Instant::now()),
            started_from_request,

            ids: SasIds {
                account: account.clone(),
                other_device: other_device.clone(),
                own_identity: own_identity.clone(),
                other_identity: other_identity.clone(),
            },

            verification_flow_id: flow_id.clone(),
            state: Arc::new(Cancelled::new(true, CancelCode::UnknownMethod)),
        };

        if let StartMethod::SasV1(method_content) = content.method() {
            let commitment = calculate_commitment(our_public_key, content);

            info!(
                public_key = our_public_key.to_base64(),
                %commitment,
                ?content,
                "Calculated SAS commitment",
            );

            if let Ok(accepted_protocols) = AcceptedProtocols::try_from(method_content) {
                Ok(SasState {
                    inner: Arc::new(Mutex::new(Some(sas))),
                    our_public_key,

                    ids: SasIds { account, other_device, other_identity, own_identity },

                    creation_time: Arc::new(Instant::now()),
                    last_event_time: Arc::new(Instant::now()),
                    started_from_request,

                    verification_flow_id: flow_id,

                    state: Arc::new(Started {
                        accepted_protocols: accepted_protocols.into(),
                        commitment,
                    }),
                })
            } else {
                Err(canceled())
            }
        } else {
            Err(canceled())
        }
    }

    pub fn into_we_accepted(self, methods: Vec<ShortAuthenticationString>) -> SasState<WeAccepted> {
        let mut accepted_protocols = self.state.accepted_protocols.as_ref().to_owned();
        accepted_protocols.short_auth_string = methods;

        // Decimal is required per spec.
        if !accepted_protocols.short_auth_string.contains(&ShortAuthenticationString::Decimal) {
            accepted_protocols.short_auth_string.push(ShortAuthenticationString::Decimal);
        }

        SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            started_from_request: self.started_from_request,
            state: Arc::new(WeAccepted {
                we_started: false,
                accepted_protocols: accepted_protocols.into(),
                commitment: self.state.commitment.clone(),
            }),
        }
    }

    fn as_content(&self) -> OwnedStartContent {
        match self.verification_flow_id.as_ref() {
            FlowId::ToDevice(s) => {
                OwnedStartContent::ToDevice(ToDeviceKeyVerificationStartEventContent::new(
                    self.device_id().into(),
                    s.clone(),
                    StartMethod::SasV1(the_protocol_definitions()),
                ))
            }
            FlowId::InRoom(r, e) => OwnedStartContent::Room(
                r.clone(),
                KeyVerificationStartEventContent::new(
                    self.device_id().into(),
                    StartMethod::SasV1(the_protocol_definitions()),
                    Relation::new(e.clone()),
                ),
            ),
        }
    }

    /// Receive a m.key.verification.accept event, changing the state into
    /// an Accepted one.
    ///
    /// Note: Even though the other side has started the (or rather "a") sas
    /// verification, it can still accept one, if we have sent one
    /// simultaneously. In this case we just go on with the verification
    /// that *we* started.
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.accept event that was sent to us by
    /// the other side.
    pub fn into_accepted(
        self,
        sender: &UserId,
        content: &AcceptContent<'_>,
    ) -> Result<SasState<Accepted>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        if let AcceptMethod::SasV1(content) = content.method() {
            let accepted_protocols = AcceptedProtocols::try_from(content.clone())
                .map_err(|c| self.clone().cancel(true, c))?;

            let start_content = self.as_content().into();

            Ok(SasState {
                inner: self.inner,
                our_public_key: self.our_public_key,
                ids: self.ids,
                verification_flow_id: self.verification_flow_id,
                creation_time: self.creation_time,
                last_event_time: Instant::now().into(),
                started_from_request: self.started_from_request,
                state: Arc::new(Accepted {
                    start_content,
                    commitment: content.commitment.clone(),
                    accepted_protocols: accepted_protocols.into(),
                }),
            })
        } else {
            Err(self.cancel(true, CancelCode::UnknownMethod))
        }
    }
}

impl SasState<WeAccepted> {
    /// Get the content for the accept event.
    ///
    /// The content needs to be sent to the other device.
    ///
    /// This should be sent out automatically if the SAS verification flow has
    /// been started because of a
    /// m.key.verification.request -> m.key.verification.ready flow.
    pub fn as_content(&self) -> OwnedAcceptContent {
        let method = AcceptMethod::SasV1(
            AcceptV1ContentInit {
                commitment: self.state.commitment.clone(),
                hash: self.state.accepted_protocols.hash.clone(),
                key_agreement_protocol: self
                    .state
                    .accepted_protocols
                    .key_agreement_protocol
                    .clone(),
                message_authentication_code: self
                    .state
                    .accepted_protocols
                    .message_auth_code
                    .clone(),
                short_authentication_string: self
                    .state
                    .accepted_protocols
                    .short_auth_string
                    .clone(),
            }
            .into(),
        );

        match self.verification_flow_id.as_ref() {
            FlowId::ToDevice(s) => {
                ToDeviceKeyVerificationAcceptEventContent::new(s.clone(), method).into()
            }
            FlowId::InRoom(r, e) => (
                r.clone(),
                KeyVerificationAcceptEventContent::new(method, Relation::new(e.clone())),
            )
                .into(),
        }
    }

    /// Receive a m.key.verification.key event, changing the state into
    /// a `KeyReceived` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.key event that was sent to us by
    /// the other side. The event will be modified so it doesn't contain any key
    /// anymore.
    pub fn into_key_received(
        self,
        sender: &UserId,
        content: &KeyContent<'_>,
    ) -> Result<SasState<KeyReceived>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        let their_public_key = Curve25519PublicKey::from_slice(content.public_key().as_bytes())
            .map_err(|_| self.clone().cancel(true, "Invalid public key".into()))?;

        let sas = self.inner.lock().unwrap().take();

        if let Some(sas) = sas {
            let established = sas
                .diffie_hellman(their_public_key)
                .map_err(|_| self.clone().cancel(true, "Invalid public key".into()))?;

            Ok(SasState {
                inner: self.inner,
                our_public_key: self.our_public_key,
                ids: self.ids,
                verification_flow_id: self.verification_flow_id,
                creation_time: self.creation_time,
                last_event_time: Instant::now().into(),
                started_from_request: self.started_from_request,
                state: Arc::new(KeyReceived {
                    sas: Mutex::new(established).into(),
                    we_started: self.state.we_started,
                    accepted_protocols: self.state.accepted_protocols.clone(),
                }),
            })
        } else {
            Err(self.cancel(true, CancelCode::UnexpectedMessage))
        }
    }
}

impl SasState<Accepted> {
    /// Receive a m.key.verification.key event, changing the state into
    /// a `KeyReceived` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.key event that was sent to us by
    /// the other side. The event will be modified so it doesn't contain any key
    /// anymore.
    pub fn into_key_received(
        self,
        sender: &UserId,
        content: &KeyContent<'_>,
    ) -> Result<SasState<KeyReceived>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        let their_public_key = Curve25519PublicKey::from_slice(content.public_key().as_bytes())
            .map_err(|_| self.clone().cancel(true, "Invalid public key".into()))?;

        let commitment =
            calculate_commitment(their_public_key, &self.state.start_content.as_start_content());

        if self.state.commitment != commitment {
            Err(self.cancel(true, CancelCode::InvalidMessage))
        } else {
            let sas = self.inner.lock().unwrap().take();

            if let Some(sas) = sas {
                let established = sas
                    .diffie_hellman(their_public_key)
                    .map_err(|_| self.clone().cancel(true, "Invalid public key".into()))?;

                Ok(SasState {
                    inner: self.inner,
                    our_public_key: self.our_public_key,
                    ids: self.ids,
                    verification_flow_id: self.verification_flow_id,
                    creation_time: self.creation_time,
                    last_event_time: Instant::now().into(),
                    started_from_request: self.started_from_request,
                    state: Arc::new(KeyReceived {
                        sas: Mutex::new(established).into(),
                        we_started: true,
                        accepted_protocols: self.state.accepted_protocols.clone(),
                    }),
                })
            } else {
                Err(self.cancel(true, CancelCode::UnexpectedMessage))
            }
        }
    }

    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side.
    pub fn as_content(&self) -> OutgoingContent {
        match &*self.verification_flow_id {
            FlowId::ToDevice(s) => AnyToDeviceEventContent::KeyVerificationKey(
                ToDeviceKeyVerificationKeyEventContent::new(
                    s.clone(),
                    Base64::new(self.our_public_key.to_vec()),
                ),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageLikeEventContent::KeyVerificationKey(
                    KeyVerificationKeyEventContent::new(
                        Base64::new(self.our_public_key.to_vec()),
                        Relation::new(e.clone()),
                    ),
                ),
            )
                .into(),
        }
    }
}

impl SasState<KeyReceived> {
    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side if and only
    /// if we_started is false.
    pub fn as_content(&self) -> OutgoingContent {
        match &*self.verification_flow_id {
            FlowId::ToDevice(s) => AnyToDeviceEventContent::KeyVerificationKey(
                ToDeviceKeyVerificationKeyEventContent::new(
                    s.clone(),
                    Base64::new(self.our_public_key.to_vec()),
                ),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageLikeEventContent::KeyVerificationKey(
                    KeyVerificationKeyEventContent::new(
                        Base64::new(self.our_public_key.to_vec()),
                        Relation::new(e.clone()),
                    ),
                ),
            )
                .into(),
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a seven tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    pub fn get_emoji(&self) -> [Emoji; 7] {
        get_emoji(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the index of the emoji of the short authentication string.
    ///
    /// Returns seven u8 numbers in the range from 0 to 63 inclusive, those
    /// numbers can be converted to a unique emoji defined by the spec.
    pub fn get_emoji_index(&self) -> [u8; 7] {
        get_emoji_index(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the decimal version of the short authentication string.
    ///
    /// Returns a tuple containing three 4 digit integer numbers that represent
    /// the short auth string.
    pub fn get_decimal(&self) -> (u16, u16, u16) {
        get_decimal(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Receive a m.key.verification.mac event, changing the state into
    /// a `MacReceived` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_mac_received(
        self,
        sender: &UserId,
        content: &MacContent<'_>,
    ) -> Result<SasState<MacReceived>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        let (devices, master_keys) = receive_mac_event(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            sender,
            content,
        )
        .map_err(|c| self.clone().cancel(true, c))?;

        Ok(SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            last_event_time: Instant::now().into(),
            ids: self.ids,
            started_from_request: self.started_from_request,
            state: Arc::new(MacReceived {
                sas: self.state.sas.clone(),
                we_started: self.state.we_started,
                verified_devices: devices.into(),
                verified_master_keys: master_keys.into(),
                accepted_protocols: self.state.accepted_protocols.clone(),
            }),
        })
    }

    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Confirmed`
    /// state.
    pub fn confirm(self) -> SasState<Confirmed> {
        SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            started_from_request: self.started_from_request,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            ids: self.ids,
            state: Arc::new(Confirmed {
                sas: self.state.sas.clone(),
                accepted_protocols: self.state.accepted_protocols.clone(),
            }),
        }
    }
}

impl SasState<Confirmed> {
    /// Receive a m.key.verification.mac event, changing the state into
    /// a `Done` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_done(
        self,
        sender: &UserId,
        content: &MacContent<'_>,
    ) -> Result<SasState<Done>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        let (devices, master_keys) = receive_mac_event(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            sender,
            content,
        )
        .map_err(|c| self.clone().cancel(true, c))?;

        Ok(SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            creation_time: self.creation_time,
            last_event_time: Instant::now().into(),
            verification_flow_id: self.verification_flow_id,
            started_from_request: self.started_from_request,
            ids: self.ids,

            state: Arc::new(Done {
                sas: self.state.sas.clone(),
                verified_devices: devices.into(),
                verified_master_keys: master_keys.into(),
            }),
        })
    }

    /// Receive a m.key.verification.mac event, changing the state into
    /// a `WaitingForDone` one. This method should be used instead of
    /// `into_done()` if the verification started with a
    /// `m.key.verification.request`.
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_waiting_for_done(
        self,
        sender: &UserId,
        content: &MacContent<'_>,
    ) -> Result<SasState<WaitingForDone>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        let (devices, master_keys) = receive_mac_event(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            sender,
            content,
        )
        .map_err(|c| self.clone().cancel(true, c))?;

        Ok(SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            creation_time: self.creation_time,
            last_event_time: Instant::now().into(),
            verification_flow_id: self.verification_flow_id,
            started_from_request: self.started_from_request,
            ids: self.ids,

            state: Arc::new(WaitingForDone {
                sas: self.state.sas.clone(),
                verified_devices: devices.into(),
                verified_master_keys: master_keys.into(),
            }),
        })
    }

    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side.
    pub fn as_content(&self) -> OutgoingContent {
        get_mac_content(&self.state.sas.lock().unwrap(), &self.ids, &self.verification_flow_id)
    }
}

impl SasState<MacReceived> {
    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Done`
    /// state since the other side already confirmed and sent us a MAC event.
    pub fn confirm(self) -> SasState<Done> {
        SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            started_from_request: self.started_from_request,
            last_event_time: self.last_event_time,
            ids: self.ids,
            state: Arc::new(Done {
                sas: self.state.sas.clone(),
                verified_devices: self.state.verified_devices.clone(),
                verified_master_keys: self.state.verified_master_keys.clone(),
            }),
        }
    }

    /// Confirm that the short auth string matches but wait for the other side
    /// to confirm that it's done.
    ///
    /// This needs to be done by the user, this will put us in the `WaitForDone`
    /// state where we wait for the other side to confirm that the MAC event was
    /// successfully received.
    pub fn confirm_and_wait_for_done(self) -> SasState<WaitingForDone> {
        SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            started_from_request: self.started_from_request,
            last_event_time: self.last_event_time,
            ids: self.ids,
            state: Arc::new(WaitingForDone {
                sas: self.state.sas.clone(),
                verified_devices: self.state.verified_devices.clone(),
                verified_master_keys: self.state.verified_master_keys.clone(),
            }),
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a vector of tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    pub fn get_emoji(&self) -> [Emoji; 7] {
        get_emoji(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the index of the emoji of the short authentication string.
    ///
    /// Returns seven u8 numbers in the range from 0 to 63 inclusive, those
    /// numbers can be converted to a unique emoji defined by the spec.
    pub fn get_emoji_index(&self) -> [u8; 7] {
        get_emoji_index(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the decimal version of the short authentication string.
    ///
    /// Returns a tuple containing three 4 digit integer numbers that represent
    /// the short auth string.
    pub fn get_decimal(&self) -> (u16, u16, u16) {
        get_decimal(
            &self.state.sas.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }
}

impl SasState<WaitingForDone> {
    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side if it
    /// wasn't already sent.
    pub fn as_content(&self) -> OutgoingContent {
        get_mac_content(&self.state.sas.lock().unwrap(), &self.ids, &self.verification_flow_id)
    }

    pub fn done_content(&self) -> OutgoingContent {
        match self.verification_flow_id.as_ref() {
            FlowId::ToDevice(t) => AnyToDeviceEventContent::KeyVerificationDone(
                ToDeviceKeyVerificationDoneEventContent::new(t.to_owned()),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageLikeEventContent::KeyVerificationDone(
                    KeyVerificationDoneEventContent::new(Relation::new(e.clone())),
                ),
            )
                .into(),
        }
    }

    /// Receive a m.key.verification.mac event, changing the state into
    /// a `Done` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_done(
        self,
        sender: &UserId,
        content: &DoneContent<'_>,
    ) -> Result<SasState<Done>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(true, c))?;

        Ok(SasState {
            inner: self.inner,
            our_public_key: self.our_public_key,
            creation_time: self.creation_time,
            last_event_time: Instant::now().into(),
            verification_flow_id: self.verification_flow_id,
            started_from_request: self.started_from_request,
            ids: self.ids,

            state: Arc::new(Done {
                sas: self.state.sas.clone(),
                verified_devices: self.state.verified_devices.clone(),
                verified_master_keys: self.state.verified_master_keys.clone(),
            }),
        })
    }
}

impl SasState<Done> {
    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side if it
    /// wasn't already sent.
    pub fn as_content(&self) -> OutgoingContent {
        get_mac_content(&self.state.sas.lock().unwrap(), &self.ids, &self.verification_flow_id)
    }

    /// Get the list of verified devices.
    pub fn verified_devices(&self) -> Arc<[ReadOnlyDevice]> {
        self.state.verified_devices.clone()
    }

    /// Get the list of verified identities.
    pub fn verified_identities(&self) -> Arc<[ReadOnlyUserIdentities]> {
        self.state.verified_master_keys.clone()
    }
}

impl SasState<Cancelled> {
    pub fn as_content(&self) -> OutgoingContent {
        self.state.as_content(&self.verification_flow_id)
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::{
        device_id,
        events::key::verification::{
            accept::{AcceptMethod, ToDeviceKeyVerificationAcceptEventContent},
            start::{StartMethod, ToDeviceKeyVerificationStartEventContent},
            ShortAuthenticationString,
        },
        serde::Base64,
        user_id, DeviceId, TransactionId, UserId,
    };
    use serde_json::json;

    use super::{Accepted, Created, SasState, Started, WeAccepted};
    use crate::{
        verification::{
            event_enums::{AcceptContent, KeyContent, MacContent, StartContent},
            FlowId,
        },
        ReadOnlyAccount, ReadOnlyDevice,
    };

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("JLAFKJWSCS")
    }

    fn bob_id() -> &'static UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> &'static DeviceId {
        device_id!("BOBDEVCIE")
    }

    async fn get_sas_pair() -> (SasState<Created>, SasState<WeAccepted>) {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        let flow_id = TransactionId::new().into();
        let alice_sas =
            SasState::<Created>::new(alice.clone(), bob_device, None, None, flow_id, false);

        let start_content = alice_sas.as_content();
        let flow_id = start_content.flow_id();

        let bob_sas = SasState::<Started>::from_start_event(
            bob.clone(),
            alice_device,
            None,
            None,
            flow_id,
            &start_content.as_start_content(),
            false,
        );
        let bob_sas = bob_sas.unwrap().into_we_accepted(vec![ShortAuthenticationString::Emoji]);

        (alice_sas, bob_sas)
    }

    #[async_test]
    async fn create_sas() {
        let (_, _) = get_sas_pair().await;
    }

    #[async_test]
    async fn sas_accept() {
        let (alice, bob) = get_sas_pair().await;
        let content = bob.as_content();
        let content = AcceptContent::from(&content);

        alice.into_accepted(bob.user_id(), &content).unwrap();
    }

    #[async_test]
    async fn sas_key_share() {
        let (alice, bob) = get_sas_pair().await;

        let content = bob.as_content();
        let content = AcceptContent::from(&content);

        let alice: SasState<Accepted> = alice.into_accepted(bob.user_id(), &content).unwrap();
        let content = alice.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let bob = bob.into_key_received(alice.user_id(), &content).unwrap();

        let content = bob.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let alice = alice.into_key_received(bob.user_id(), &content).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());
    }

    #[async_test]
    async fn sas_full() {
        let (alice, bob) = get_sas_pair().await;

        let content = bob.as_content();
        let content = AcceptContent::from(&content);

        let alice: SasState<Accepted> = alice.into_accepted(bob.user_id(), &content).unwrap();
        let content = alice.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let bob = bob.into_key_received(alice.user_id(), &content).unwrap();

        let content = bob.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let alice = alice.into_key_received(bob.user_id(), &content).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());

        let bob_decimals = bob.get_decimal();

        let bob = bob.confirm();

        let content = bob.as_content();
        let content = MacContent::try_from(&content).unwrap();

        let alice = alice.into_mac_received(bob.user_id(), &content).unwrap();
        assert!(!alice.get_emoji().is_empty());
        assert_eq!(alice.get_decimal(), bob_decimals);
        let alice = alice.confirm();

        let content = alice.as_content();
        let content = MacContent::try_from(&content).unwrap();
        let bob = bob.into_done(alice.user_id(), &content).unwrap();

        assert!(bob.verified_devices().contains(&bob.other_device()));
        assert!(alice.verified_devices().contains(&alice.other_device()));
    }

    #[async_test]
    async fn sas_invalid_commitment() {
        let (alice, bob) = get_sas_pair().await;

        let mut content = bob.as_content();
        let mut method = content.method_mut();

        match &mut method {
            AcceptMethod::SasV1(c) => {
                c.commitment = Base64::empty();
            }
            _ => panic!("Unknown accept event content"),
        }

        let content = AcceptContent::from(&content);

        let alice: SasState<Accepted> = alice.into_accepted(bob.user_id(), &content).unwrap();

        let content = alice.as_content();
        let content = KeyContent::try_from(&content).unwrap();
        let bob = bob.into_key_received(alice.user_id(), &content).unwrap();
        let content = bob.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        alice
            .into_key_received(bob.user_id(), &content)
            .expect_err("Didn't cancel on invalid commitment");
    }

    #[async_test]
    async fn sas_invalid_sender() {
        let (alice, bob) = get_sas_pair().await;

        let content = bob.as_content();
        let content = AcceptContent::from(&content);
        let sender = user_id!("@malory:example.org");
        alice.into_accepted(sender, &content).expect_err("Didn't cancel on a invalid sender");
    }

    #[async_test]
    async fn sas_unknown_sas_method() {
        let (alice, bob) = get_sas_pair().await;

        let mut content = bob.as_content();
        let mut method = content.method_mut();

        match &mut method {
            AcceptMethod::SasV1(ref mut c) => {
                c.short_authentication_string = vec![];
            }
            _ => panic!("Unknown accept event content"),
        }

        let content = AcceptContent::from(&content);

        alice
            .into_accepted(bob.user_id(), &content)
            .expect_err("Didn't cancel on an invalid SAS method");
    }

    #[async_test]
    async fn sas_unknown_method() {
        let (alice, bob) = get_sas_pair().await;

        let content = json!({
            "method": "m.sas.custom",
            "method_data": "something",
            "transaction_id": "some_id",
        });

        let content: ToDeviceKeyVerificationAcceptEventContent =
            serde_json::from_value(content).unwrap();
        let content = AcceptContent::from(&content);

        alice
            .into_accepted(bob.user_id(), &content)
            .expect_err("Didn't cancel on an unknown SAS method");
    }

    #[async_test]
    async fn sas_from_start_unknown_method() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        let flow_id = TransactionId::new().into();
        let alice_sas =
            SasState::<Created>::new(alice.clone(), bob_device, None, None, flow_id, false);

        let mut start_content = alice_sas.as_content();
        let method = start_content.method_mut();

        match method {
            StartMethod::SasV1(ref mut c) => {
                c.message_authentication_codes = vec![];
            }
            _ => panic!("Unknown SAS start method"),
        }

        let flow_id = start_content.flow_id();
        let content = StartContent::from(&start_content);

        SasState::<Started>::from_start_event(
            bob.clone(),
            alice_device.clone(),
            None,
            None,
            flow_id,
            &content,
            false,
        )
        .expect_err("Didn't cancel on invalid MAC method");

        let content = json!({
            "method": "m.sas.custom",
            "from_device": "DEVICEID",
            "method_data": "something",
            "transaction_id": "some_id",
        });

        let content: ToDeviceKeyVerificationStartEventContent =
            serde_json::from_value(content).unwrap();
        let content = StartContent::from(&content);
        let flow_id = content.flow_id().to_owned();

        SasState::<Started>::from_start_event(
            bob.clone(),
            alice_device,
            None,
            None,
            FlowId::ToDevice(flow_id.into()),
            &content,
            false,
        )
        .expect_err("Didn't cancel on unknown sas method");
    }
}
