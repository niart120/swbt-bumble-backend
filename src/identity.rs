// Originally implemented in niart120/swbt-rs at b61476f. Moved and modified
// so CSR volatile-address rewrite, USB re-enumeration, and readback form one
// fail-closed backend operation. See PROVENANCE.md.

use std::{error::Error as StdError, fmt, time::Duration};

use super::csr::{
    CsrVendorCommand, build_csr_bd_addr_volatile_rewrite_plan, parse_csr_bccmd_response,
};

const CSR_COMPANY_IDENTIFIER: u16 = 10;

pub(super) trait AdapterIdentitySession {
    type Error: StdError + Send + Sync + 'static;

    fn initialize(&mut self, response_timeout: Duration) -> Result<u16, Self::Error>;

    fn read_address(&mut self, response_timeout: Duration) -> Result<[u8; 6], Self::Error>;

    fn send_vendor_command(
        &mut self,
        command: &CsrVendorCommand,
        response_timeout: Duration,
    ) -> Result<Box<[u8]>, Self::Error>;

    fn send_command_without_response(
        &mut self,
        command: &CsrVendorCommand,
    ) -> Result<(), Self::Error>;

    fn close(&mut self) -> Result<(), Self::Error>;
}

pub(super) trait AdapterIdentityBackend {
    type Error: StdError + Send + Sync + 'static;
    type Session: AdapterIdentitySession<Error = Self::Error>;

    fn open(&mut self) -> Result<Self::Session, Self::Error>;

    fn now(&self) -> Duration;

    fn sleep(&mut self, duration: Duration);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IdentityPreparationOptions {
    pub(super) response_timeout: Duration,
    pub(super) reenumeration_timeout: Duration,
    pub(super) reenumeration_poll_interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AdapterIdentityPreparation {
    AlreadyActive,
    Rewritten,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IdentityPreparationErrorKind {
    UnsupportedController,
    FailedBeforeWrite,
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IdentityPreparationStage {
    Open,
    Initialize,
    ReadCurrent,
    Write,
    Close,
    Reenumeration,
    Readback,
}

pub(super) struct IdentityPreparationError {
    kind: IdentityPreparationErrorKind,
    stage: IdentityPreparationStage,
}

impl IdentityPreparationError {
    pub(super) const fn kind(&self) -> IdentityPreparationErrorKind {
        self.kind
    }

    #[cfg(test)]
    pub(super) const fn stage(&self) -> IdentityPreparationStage {
        self.stage
    }

    fn without_source(kind: IdentityPreparationErrorKind, stage: IdentityPreparationStage) -> Self {
        Self { kind, stage }
    }

    fn with_source(
        kind: IdentityPreparationErrorKind,
        stage: IdentityPreparationStage,
        _source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self { kind, stage }
    }
}

impl fmt::Debug for IdentityPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityPreparationError")
            .field("kind", &self.kind)
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for IdentityPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            IdentityPreparationErrorKind::UnsupportedController => {
                "adapter does not support explicit local address preparation"
            }
            IdentityPreparationErrorKind::FailedBeforeWrite => {
                "adapter identity preparation failed before writing"
            }
            IdentityPreparationErrorKind::RecoveryRequired => {
                "adapter identity is uncertain; power cycle the USB adapter before retrying"
            }
        })
    }
}

impl StdError for IdentityPreparationError {}

pub(super) fn prepare_adapter_identity<B: AdapterIdentityBackend>(
    backend: &mut B,
    target: [u8; 6],
    options: IdentityPreparationOptions,
) -> Result<AdapterIdentityPreparation, IdentityPreparationError> {
    let mut session = backend.open().map_err(|source| {
        IdentityPreparationError::with_source(
            IdentityPreparationErrorKind::FailedBeforeWrite,
            IdentityPreparationStage::Open,
            source,
        )
    })?;

    let company_identifier = match session.initialize(options.response_timeout) {
        Ok(company_identifier) => company_identifier,
        Err(source) => {
            let error = IdentityPreparationError::with_source(
                IdentityPreparationErrorKind::FailedBeforeWrite,
                IdentityPreparationStage::Initialize,
                source,
            );
            let _ = session.close();
            return Err(error);
        }
    };
    if company_identifier != CSR_COMPANY_IDENTIFIER {
        let _ = session.close();
        return Err(IdentityPreparationError::without_source(
            IdentityPreparationErrorKind::UnsupportedController,
            IdentityPreparationStage::Initialize,
        ));
    }

    let current = match session.read_address(options.response_timeout) {
        Ok(address) => address,
        Err(source) => {
            let error = IdentityPreparationError::with_source(
                IdentityPreparationErrorKind::FailedBeforeWrite,
                IdentityPreparationStage::ReadCurrent,
                source,
            );
            let _ = session.close();
            return Err(error);
        }
    };
    if current == target {
        session.close().map_err(|source| {
            IdentityPreparationError::with_source(
                IdentityPreparationErrorKind::FailedBeforeWrite,
                IdentityPreparationStage::Close,
                source,
            )
        })?;
        return Ok(AdapterIdentityPreparation::AlreadyActive);
    }

    let rewrite = build_csr_bd_addr_volatile_rewrite_plan(target, 0x4711);
    let response = match session.send_vendor_command(rewrite.write(), options.response_timeout) {
        Ok(response) => response,
        Err(source) => {
            let error = recovery_with_source(IdentityPreparationStage::Write, source);
            let _ = session.close();
            return Err(error);
        }
    };
    if let Err(source) = parse_csr_bccmd_response(&response).and_then(|status| {
        if status == 0 {
            Ok(())
        } else {
            Err(super::csr::CsrCodecError::FailedResponse)
        }
    }) {
        let error = recovery_with_source(IdentityPreparationStage::Write, source);
        let _ = session.close();
        return Err(error);
    }
    // CSR warm reset normally removes the USB device before the synchronous
    // control transfer or reader shutdown can report success. Settle both
    // results by reopening and reading the identity instead of treating either
    // old-handle result as final.
    let _ = session.send_command_without_response(rewrite.reset());
    let _ = session.close();

    let mut readback = open_reenumerated_session(backend, options)?;
    let readback_company = match readback.initialize(options.response_timeout) {
        Ok(company_identifier) => company_identifier,
        Err(source) => {
            let error = recovery_with_source(IdentityPreparationStage::Readback, source);
            let _ = readback.close();
            return Err(error);
        }
    };
    if readback_company != CSR_COMPANY_IDENTIFIER {
        let _ = readback.close();
        return Err(recovery_without_source(IdentityPreparationStage::Readback));
    }
    let readback_address = match readback.read_address(options.response_timeout) {
        Ok(address) => address,
        Err(source) => {
            let error = recovery_with_source(IdentityPreparationStage::Readback, source);
            let _ = readback.close();
            return Err(error);
        }
    };
    readback
        .close()
        .map_err(|source| recovery_with_source(IdentityPreparationStage::Close, source))?;
    if readback_address != target {
        return Err(recovery_without_source(IdentityPreparationStage::Readback));
    }

    Ok(AdapterIdentityPreparation::Rewritten)
}

fn open_reenumerated_session<B: AdapterIdentityBackend>(
    backend: &mut B,
    options: IdentityPreparationOptions,
) -> Result<B::Session, IdentityPreparationError> {
    let deadline = backend.now().saturating_add(options.reenumeration_timeout);
    let poll_interval = options
        .reenumeration_poll_interval
        .max(Duration::from_millis(1));
    loop {
        backend.sleep(poll_interval);
        match backend.open() {
            Ok(session) => return Ok(session),
            Err(source) if backend.now() < deadline => drop(source),
            Err(source) => {
                return Err(recovery_with_source(
                    IdentityPreparationStage::Reenumeration,
                    source,
                ));
            }
        }
    }
}

fn recovery_without_source(stage: IdentityPreparationStage) -> IdentityPreparationError {
    IdentityPreparationError::without_source(IdentityPreparationErrorKind::RecoveryRequired, stage)
}

fn recovery_with_source(
    stage: IdentityPreparationStage,
    source: impl StdError + Send + Sync + 'static,
) -> IdentityPreparationError {
    IdentityPreparationError::with_source(
        IdentityPreparationErrorKind::RecoveryRequired,
        stage,
        source,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        error::Error as StdError,
        fmt,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::{
        AdapterIdentityBackend, AdapterIdentityPreparation, AdapterIdentitySession,
        IdentityPreparationErrorKind, IdentityPreparationOptions, IdentityPreparationStage,
        prepare_adapter_identity,
    };
    use crate::csr::CsrVendorCommand;

    const ORIGINAL: [u8; 6] = [0x00, 0x1B, 0xDC, 0xF9, 0x9F, 0x7D];
    const TARGET: [u8; 6] = [0x02, 0x12, 0x34, 0x56, 0x78, 0x9A];

    #[test]
    fn already_active_closes_without_write_reset_or_reenumeration() {
        let (mut backend, events) =
            ScriptedBackend::new([OpenStep::session("current", 10, TARGET)]);

        let result = prepare_adapter_identity(&mut backend, TARGET, options())
            .expect("matching active address needs no rewrite");

        assert_eq!(result, AdapterIdentityPreparation::AlreadyActive);
        assert_eq!(
            event_snapshot(&events),
            [
                "current:open",
                "current:initialize",
                "current:read_address",
                "current:close",
            ]
        );
    }

    #[test]
    fn rewrite_waits_for_reenumeration_and_requires_matching_readback() {
        let (mut backend, events) = ScriptedBackend::new([
            OpenStep::session("before", 10, ORIGINAL),
            OpenStep::failure("reenumeration_open_failed"),
            OpenStep::session("after", 10, TARGET),
        ]);

        let result = prepare_adapter_identity(&mut backend, TARGET, options())
            .expect("matching readback completes preparation");

        assert_eq!(result, AdapterIdentityPreparation::Rewritten);
        assert_eq!(
            event_snapshot(&events),
            [
                "before:open",
                "before:initialize",
                "before:read_address",
                "before:write",
                "before:warm_reset",
                "before:close",
                "sleep",
                "reenumeration_open_failed",
                "sleep",
                "after:open",
                "after:initialize",
                "after:read_address",
                "after:close",
            ]
        );
    }

    #[test]
    fn reset_disconnect_close_failure_is_settled_by_reenumeration_readback() {
        let before = ScriptedSession::new("before", 10, ORIGINAL).fail_on("close");
        let (mut backend, events) = ScriptedBackend::new([
            OpenStep::Session(before),
            OpenStep::session("after", 10, TARGET),
        ]);

        let result = prepare_adapter_identity(&mut backend, TARGET, options())
            .expect("readback settles the expected reset disconnect");

        assert_eq!(result, AdapterIdentityPreparation::Rewritten);
        assert_eq!(
            event_snapshot(&events),
            [
                "before:open",
                "before:initialize",
                "before:read_address",
                "before:write",
                "before:warm_reset",
                "before:close",
                "sleep",
                "after:open",
                "after:initialize",
                "after:read_address",
                "after:close",
            ]
        );
    }

    #[test]
    fn warm_reset_transfer_failure_is_settled_by_reenumeration_readback() {
        let before = ScriptedSession::new("before", 10, ORIGINAL).fail_on("warm_reset");
        let (mut backend, events) = ScriptedBackend::new([
            OpenStep::Session(before),
            OpenStep::session("after", 10, TARGET),
        ]);

        let result = prepare_adapter_identity(&mut backend, TARGET, options())
            .expect("readback settles a transfer interrupted by warm-reset re-enumeration");

        assert_eq!(result, AdapterIdentityPreparation::Rewritten);
        assert_eq!(
            event_snapshot(&events),
            [
                "before:open",
                "before:initialize",
                "before:read_address",
                "before:write",
                "before:warm_reset",
                "before:close",
                "sleep",
                "after:open",
                "after:initialize",
                "after:read_address",
                "after:close",
            ]
        );
    }

    #[test]
    fn non_csr_controller_is_unsupported_before_any_write() {
        let (mut backend, events) =
            ScriptedBackend::new([OpenStep::session("other", 76, ORIGINAL)]);

        let error = prepare_adapter_identity(&mut backend, TARGET, options())
            .expect_err("non-CSR controller must fail closed");

        assert_eq!(
            error.kind(),
            IdentityPreparationErrorKind::UnsupportedController
        );
        assert_eq!(error.stage(), IdentityPreparationStage::Initialize);
        assert_eq!(
            event_snapshot(&events),
            ["other:open", "other:initialize", "other:close"]
        );
    }

    #[test]
    fn read_failure_before_write_remains_retryable_and_closes() {
        let session = ScriptedSession::new("before", 10, ORIGINAL).fail_on("read_address");
        let (mut backend, events) = ScriptedBackend::new([OpenStep::Session(session)]);

        let error = prepare_adapter_identity(&mut backend, TARGET, options())
            .expect_err("pre-write read failure must remain retryable");

        assert_eq!(
            error.kind(),
            IdentityPreparationErrorKind::FailedBeforeWrite
        );
        assert_eq!(error.stage(), IdentityPreparationStage::ReadCurrent);
        assert_eq!(
            event_snapshot(&events),
            [
                "before:open",
                "before:initialize",
                "before:read_address",
                "before:close",
            ]
        );
        assert_error_is_redacted(&error);
    }

    #[test]
    fn write_failure_requires_physical_recovery_and_still_closes() {
        let session = ScriptedSession::new("before", 10, ORIGINAL).fail_on("write");
        let (mut backend, events) = ScriptedBackend::new([OpenStep::Session(session)]);

        let error = prepare_adapter_identity(&mut backend, TARGET, options())
            .expect_err("write-started failure must require recovery");

        assert_eq!(error.kind(), IdentityPreparationErrorKind::RecoveryRequired);
        assert_eq!(error.stage(), IdentityPreparationStage::Write);
        assert_eq!(
            event_snapshot(&events),
            [
                "before:open",
                "before:initialize",
                "before:read_address",
                "before:write",
                "before:close",
            ]
        );
        assert_error_is_redacted(&error);
    }

    #[test]
    fn reenumeration_and_readback_failures_require_recovery() {
        let before = ScriptedSession::new("before", 10, ORIGINAL).fail_on("warm_reset");
        let (mut timeout_backend, timeout_events) = ScriptedBackend::new([
            OpenStep::Session(before),
            OpenStep::failure("reenumeration_open_failed"),
        ]);
        let timeout_error = prepare_adapter_identity(
            &mut timeout_backend,
            TARGET,
            IdentityPreparationOptions {
                reenumeration_timeout: Duration::ZERO,
                ..options()
            },
        )
        .expect_err("re-enumeration timeout must require recovery");
        assert_eq!(
            timeout_error.kind(),
            IdentityPreparationErrorKind::RecoveryRequired
        );
        assert_eq!(
            timeout_error.stage(),
            IdentityPreparationStage::Reenumeration
        );
        assert!(event_snapshot(&timeout_events).contains(&"sleep"));

        let before = ScriptedSession::new("before", 10, ORIGINAL).fail_on("warm_reset");
        let (mut mismatch_backend, mismatch_events) = ScriptedBackend::new([
            OpenStep::Session(before),
            OpenStep::session("after", 10, ORIGINAL),
        ]);
        let mismatch_error = prepare_adapter_identity(&mut mismatch_backend, TARGET, options())
            .expect_err("wrong readback address must require recovery");
        assert_eq!(
            mismatch_error.kind(),
            IdentityPreparationErrorKind::RecoveryRequired
        );
        assert_eq!(mismatch_error.stage(), IdentityPreparationStage::Readback);
        assert!(event_snapshot(&mismatch_events).contains(&"after:close"));
        assert_error_is_redacted(&mismatch_error);

        let readback = ScriptedSession::new("after", 10, TARGET).fail_on("close");
        let (mut close_backend, close_events) = ScriptedBackend::new([
            OpenStep::session("before", 10, ORIGINAL),
            OpenStep::Session(readback),
        ]);
        let close_error = prepare_adapter_identity(&mut close_backend, TARGET, options())
            .expect_err("readback session close failure remains uncertain");
        assert_eq!(
            close_error.kind(),
            IdentityPreparationErrorKind::RecoveryRequired
        );
        assert_eq!(close_error.stage(), IdentityPreparationStage::Close);
        assert!(event_snapshot(&close_events).contains(&"after:close"));
        assert_error_is_redacted(&close_error);
    }

    fn options() -> IdentityPreparationOptions {
        IdentityPreparationOptions {
            response_timeout: Duration::from_secs(2),
            reenumeration_timeout: Duration::from_secs(1),
            reenumeration_poll_interval: Duration::from_millis(10),
        }
    }

    fn assert_error_is_redacted(error: &super::IdentityPreparationError) {
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains("02:12"));
            assert!(!rendered.contains("c202"));
        }
        assert!(error.source().is_none());
    }

    fn event_snapshot(events: &Arc<Mutex<Vec<&'static str>>>) -> Vec<&'static str> {
        events.lock().expect("event lock").clone()
    }

    struct ScriptedBackend {
        steps: VecDeque<OpenStep>,
        events: Arc<Mutex<Vec<&'static str>>>,
        now: Duration,
    }

    impl ScriptedBackend {
        fn new(steps: impl IntoIterator<Item = OpenStep>) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    steps: steps.into_iter().collect(),
                    events: Arc::clone(&events),
                    now: Duration::ZERO,
                },
                events,
            )
        }
    }

    impl AdapterIdentityBackend for ScriptedBackend {
        type Error = TestError;
        type Session = ScriptedSession;

        fn open(&mut self) -> Result<Self::Session, Self::Error> {
            match self.steps.pop_front().expect("scripted open step") {
                OpenStep::Session(mut session) => {
                    session.events = Arc::clone(&self.events);
                    session.record(session.open_event);
                    Ok(session)
                }
                OpenStep::Failure(event) => {
                    self.events.lock().expect("event lock").push(event);
                    Err(TestError("secret open failure"))
                }
            }
        }

        fn now(&self) -> Duration {
            self.now
        }

        fn sleep(&mut self, duration: Duration) {
            self.events.lock().expect("event lock").push("sleep");
            self.now += duration;
        }
    }

    enum OpenStep {
        Session(ScriptedSession),
        Failure(&'static str),
    }

    impl OpenStep {
        fn session(label: &'static str, company_identifier: u16, address: [u8; 6]) -> Self {
            Self::Session(ScriptedSession::new(label, company_identifier, address))
        }

        const fn failure(event: &'static str) -> Self {
            Self::Failure(event)
        }
    }

    struct ScriptedSession {
        open_event: &'static str,
        initialize_event: &'static str,
        read_event: &'static str,
        write_event: &'static str,
        reset_event: &'static str,
        close_event: &'static str,
        company_identifier: u16,
        address: [u8; 6],
        fail_on: Option<&'static str>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ScriptedSession {
        fn new(label: &'static str, company_identifier: u16, address: [u8; 6]) -> Self {
            let (open_event, initialize_event, read_event, write_event, reset_event, close_event) =
                match label {
                    "current" => (
                        "current:open",
                        "current:initialize",
                        "current:read_address",
                        "current:write",
                        "current:warm_reset",
                        "current:close",
                    ),
                    "before" => (
                        "before:open",
                        "before:initialize",
                        "before:read_address",
                        "before:write",
                        "before:warm_reset",
                        "before:close",
                    ),
                    "after" => (
                        "after:open",
                        "after:initialize",
                        "after:read_address",
                        "after:write",
                        "after:warm_reset",
                        "after:close",
                    ),
                    "other" => (
                        "other:open",
                        "other:initialize",
                        "other:read_address",
                        "other:write",
                        "other:warm_reset",
                        "other:close",
                    ),
                    _ => panic!("unknown test session label"),
                };
            Self {
                open_event,
                initialize_event,
                read_event,
                write_event,
                reset_event,
                close_event,
                company_identifier,
                address,
                fail_on: None,
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn fail_on(mut self, operation: &'static str) -> Self {
            self.fail_on = Some(operation);
            self
        }

        fn record(&self, event: &'static str) {
            self.events.lock().expect("event lock").push(event);
        }

        fn result(&self, operation: &'static str) -> Result<(), TestError> {
            if self.fail_on == Some(operation) {
                Err(TestError("secret scripted failure"))
            } else {
                Ok(())
            }
        }
    }

    impl AdapterIdentitySession for ScriptedSession {
        type Error = TestError;

        fn initialize(&mut self, response_timeout: Duration) -> Result<u16, Self::Error> {
            assert_eq!(response_timeout, Duration::from_secs(2));
            self.record(self.initialize_event);
            self.result("initialize")?;
            Ok(self.company_identifier)
        }

        fn read_address(&mut self, response_timeout: Duration) -> Result<[u8; 6], Self::Error> {
            assert_eq!(response_timeout, Duration::from_secs(2));
            self.record(self.read_event);
            self.result("read_address")?;
            Ok(self.address)
        }

        fn send_vendor_command(
            &mut self,
            command: &CsrVendorCommand,
            response_timeout: Duration,
        ) -> Result<Box<[u8]>, Self::Error> {
            assert_eq!(response_timeout, Duration::from_secs(2));
            assert_eq!(command.op_code(), 0xFC00);
            self.record(self.write_event);
            self.result("write")?;
            Ok(hex("c201000c00114703700000").into_boxed_slice())
        }

        fn send_command_without_response(
            &mut self,
            command: &CsrVendorCommand,
        ) -> Result<(), Self::Error> {
            assert_eq!(command.op_code(), 0xFC00);
            self.record(self.reset_event);
            self.result("warm_reset")
        }

        fn close(&mut self) -> Result<(), Self::Error> {
            self.record(self.close_event);
            self.result("close")
        }
    }

    #[derive(Debug)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl StdError for TestError {}

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("ASCII hex fixture");
                u8::from_str_radix(text, 16).expect("valid hex fixture")
            })
            .collect()
    }
}
