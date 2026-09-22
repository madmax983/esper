//! Static catalog, capabilities, and the policy gate (§5, §17).

use esper_core::Error;
use esper_core::decision::{Level, StatusDetail, ToolArgs};
use esper_core::ids::{Pin, ToolId};
use esper_core::registry::{
    CATALOG, Capabilities, DEVICE_STATUS_REPORT_ID, DEVICE_STATUS_REPORT_NAME, GPIO_PIN_READ_ID,
    GPIO_PIN_READ_NAME, GPIO_PIN_WRITE_ID, GPIO_PIN_WRITE_NAME, IdempotencyStrategy,
    PermissionClass, SENSOR_SAMPLE_READ_ID, SENSOR_SAMPLE_READ_NAME, TIMER_DELAY_WAIT_ID,
    TIMER_DELAY_WAIT_NAME, TIMER_UPTIME_READ_ID, TIMER_UPTIME_READ_NAME, ToolContract,
    VerificationStrategy, authorize, catalog, lookup_by_id, lookup_by_name,
};
use esper_protocol as protocol;

fn caps(read: &[u8], write: &[u8], sensors: &[u8], timer: bool, status: bool) -> Capabilities {
    let mut c = Capabilities::empty();
    for (i, pin) in read.iter().enumerate() {
        c.read_pins[i] = *pin;
    }
    c.read_count = u8::try_from(read.len()).expect("test pins fit in u8");
    for (i, pin) in write.iter().enumerate() {
        c.write_pins[i] = *pin;
    }
    c.write_count = u8::try_from(write.len()).expect("test pins fit in u8");
    for (i, sensor) in sensors.iter().enumerate() {
        c.sensors[i] = *sensor;
    }
    c.sensor_count = u8::try_from(sensors.len()).expect("test sensors fit in u8");
    c.allow_timer = timer;
    c.allow_status = status;
    c
}

fn full_caps() -> Capabilities {
    caps(&[4], &[4], &[0, 1, 2, 3], true, true)
}

fn pin4() -> Pin {
    Pin::new(4).expect("pin")
}

fn pin5() -> Pin {
    Pin::new(5).expect("pin")
}

#[test]
fn catalog_holds_exactly_the_six_e2_tools() {
    assert_eq!(CATALOG.len(), 6);
    assert_eq!(catalog().len(), 6);

    let expected = [
        (
            GPIO_PIN_READ_ID,
            GPIO_PIN_READ_NAME,
            PermissionClass::ReadOnly,
            VerificationStrategy::None,
            IdempotencyStrategy::NotApplicable,
            64,
        ),
        (
            GPIO_PIN_WRITE_ID,
            GPIO_PIN_WRITE_NAME,
            PermissionClass::IdempotentWrite,
            VerificationStrategy::ReadBack,
            IdempotencyStrategy::SetOperation,
            64,
        ),
        (
            SENSOR_SAMPLE_READ_ID,
            SENSOR_SAMPLE_READ_NAME,
            PermissionClass::ReadOnly,
            VerificationStrategy::None,
            IdempotencyStrategy::NotApplicable,
            64,
        ),
        (
            TIMER_UPTIME_READ_ID,
            TIMER_UPTIME_READ_NAME,
            PermissionClass::ReadOnly,
            VerificationStrategy::None,
            IdempotencyStrategy::NotApplicable,
            64,
        ),
        (
            TIMER_DELAY_WAIT_ID,
            TIMER_DELAY_WAIT_NAME,
            PermissionClass::IdempotentWrite,
            VerificationStrategy::ReadBack,
            IdempotencyStrategy::SetOperation,
            64,
        ),
        (
            DEVICE_STATUS_REPORT_ID,
            DEVICE_STATUS_REPORT_NAME,
            PermissionClass::ReadOnly,
            VerificationStrategy::None,
            IdempotencyStrategy::NotApplicable,
            128,
        ),
    ];
    for (entry, (id, name, permission, verification, idempotency, bound)) in
        CATALOG.iter().zip(expected)
    {
        assert_eq!(entry.id, id);
        assert_eq!(entry.name, name);
        assert_eq!(entry.permission, permission);
        assert_eq!(entry.verification, verification);
        assert_eq!(entry.idempotency, idempotency);
        assert_eq!(entry.result_bound, bound);
    }
    // Ids 1-2 keep their E0/E1 assignments and are never renumbered.
    assert_eq!(CATALOG[0].id, 1);
    assert_eq!(CATALOG[1].id, 2);
}

#[test]
fn core_reexports_the_protocol_catalog_types() {
    // Exactly one definition of each exists: these ascriptions only
    // compile when the re-exported names ARE the protocol's types.
    fn takes_permission(_: protocol::PermissionClass) {}
    fn takes_verification(_: protocol::VerificationStrategy) {}
    fn takes_idempotency(_: protocol::IdempotencyStrategy) {}
    takes_permission(PermissionClass::ReadOnly);
    takes_verification(VerificationStrategy::ReadBack);
    takes_idempotency(IdempotencyStrategy::SetOperation);
    let entry: &protocol::ToolContract = lookup_by_id(ToolId::new(1)).expect("tool 1");
    let _: &ToolContract = entry;
    assert_eq!(protocol::catalog().len(), catalog().len());
}

#[test]
fn no_tool_carries_a_sensitive_permission() {
    for entry in CATALOG {
        assert!(
            !matches!(
                entry.permission,
                PermissionClass::SensitiveWrite | PermissionClass::Irreversible
            ),
            "{} must not carry a sensitive permission",
            entry.name
        );
    }
}

#[test]
fn lookup_round_trips() {
    let read = lookup_by_id(ToolId::new(1)).expect("tool 1");
    assert_eq!(read.name, "gpio_pin_read");
    for id in 1..=6u8 {
        let entry = lookup_by_id(ToolId::new(id)).expect("catalog tool");
        assert_eq!(entry.id, id);
        assert_eq!(
            lookup_by_name(entry.name.as_bytes()).expect("by name").id,
            id
        );
    }
    assert!(lookup_by_id(ToolId::new(7)).is_none());

    assert_eq!(
        lookup_by_name(b"gpio_pin_read").expect("by name").id,
        GPIO_PIN_READ_ID
    );
    assert_eq!(
        lookup_by_name(b"device_status_report").expect("by name").id,
        DEVICE_STATUS_REPORT_ID
    );
    assert!(lookup_by_name(b"gpio_frobnicator").is_none());
    assert!(lookup_by_name(b"").is_none());
    assert!(
        lookup_by_name(b"gpio_pin_read\xff").is_none(),
        "non-UTF8 names never match"
    );
}

#[test]
fn capabilities_gate_reads_writes_samples_timers_and_status() {
    let c = caps(&[4], &[4], &[2], true, true);
    assert!(c.can_read(pin4()));
    assert!(!c.can_read(pin5()));
    assert!(c.can_write(pin4()));
    assert!(!c.can_write(pin5()));
    assert!(c.can_sample(2));
    assert!(!c.can_sample(3));
    assert!(c.allow_timer);
    assert!(c.allow_status);
}

#[test]
fn empty_capabilities_deny_everything() {
    let c = Capabilities::empty();
    for pin in 0..8u8 {
        let pin = Pin::new(pin).expect("pin");
        assert!(!c.can_read(pin));
        assert!(!c.can_write(pin));
    }
    for sensor in 0..4u8 {
        assert!(!c.can_sample(sensor));
    }
    assert!(!c.allow_timer);
    assert!(!c.allow_status);
}

#[test]
fn counts_beyond_storage_fail_closed() {
    let mut c = caps(&[4], &[], &[], false, false);
    c.read_count = 9; // claims more pins than storage holds
    assert!(!c.can_read(pin4()));
    c.sensor_count = 5; // claims more sensors than storage holds
    assert!(!c.can_sample(0));
}

#[test]
fn widens_detects_added_pins() {
    let seed = caps(&[4], &[4], &[1], true, false);
    assert!(!caps(&[4], &[4], &[1], true, false).widens(&seed));
    assert!(
        !caps(&[], &[4], &[1], true, false).widens(&seed),
        "shrinking is not widening"
    );
    assert!(!caps(&[], &[], &[], false, false).widens(&seed));

    let mut wider = caps(&[4], &[4], &[1], true, false);
    wider.read_pins[1] = 5;
    wider.read_count = 2;
    assert!(wider.widens(&seed), "added read pin widens");

    let mut wider_write = caps(&[4], &[4], &[1], true, false);
    wider_write.write_pins[1] = 6;
    wider_write.write_count = 2;
    assert!(wider_write.widens(&seed), "added write pin widens");
}

#[test]
fn widens_detects_added_sensors_and_grants() {
    let seed = caps(&[4], &[4], &[1], false, false);

    let mut wider_sensor = caps(&[4], &[4], &[1], false, false);
    wider_sensor.sensors[1] = 2;
    wider_sensor.sensor_count = 2;
    assert!(wider_sensor.widens(&seed), "added sensor widens");

    let wider_timer = caps(&[4], &[4], &[1], true, false);
    assert!(wider_timer.widens(&seed), "added timer grant widens");

    let wider_status = caps(&[4], &[4], &[1], false, true);
    assert!(wider_status.widens(&seed), "added status grant widens");

    // Narrowing a grant is not widening.
    let narrower = caps(&[4], &[4], &[1], false, false);
    assert!(!narrower.widens(&caps(&[4], &[4], &[1], true, true)));
}

#[test]
fn widens_fails_closed_on_malformed_counts() {
    let seed = caps(&[4], &[4], &[1], false, false);
    let mut malformed = caps(&[4], &[4], &[1], false, false);
    malformed.sensor_count = 9;
    assert!(
        malformed.widens(&seed),
        "malformed sensor count widens (fail closed)"
    );
    let mut malformed_seed = seed;
    malformed_seed.read_count = 200;
    assert!(
        seed.widens(&malformed_seed),
        "malformed baseline count widens (fail closed)"
    );
}

// --- `authorize`: allowlist → capability (§5.2 v2). Device business
// rules live in the runtime; this gate does capability only. ---

fn entry(id: u8) -> &'static ToolContract {
    lookup_by_id(ToolId::new(id)).expect("catalog tool")
}

#[test]
fn authorize_allows_granted_calls() {
    let c = full_caps();
    authorize(
        &c,
        entry(GPIO_PIN_READ_ID),
        &ToolArgs::GpioPinRead { pin: pin4() },
    )
    .expect("read allowed");
    authorize(
        &c,
        entry(GPIO_PIN_WRITE_ID),
        &ToolArgs::GpioPinWrite {
            pin: pin4(),
            level: Level::High,
        },
    )
    .expect("write allowed");
    authorize(
        &c,
        entry(SENSOR_SAMPLE_READ_ID),
        &ToolArgs::SensorSampleRead { sensor: 2 },
    )
    .expect("sensor allowed");
    authorize(&c, entry(TIMER_UPTIME_READ_ID), &ToolArgs::TimerUptimeRead).expect("uptime allowed");
    authorize(
        &c,
        entry(TIMER_DELAY_WAIT_ID),
        &ToolArgs::TimerDelayWait { ms: 250 },
    )
    .expect("delay allowed");
    authorize(
        &c,
        entry(DEVICE_STATUS_REPORT_ID),
        &ToolArgs::DeviceStatusReport {
            detail: StatusDetail::Summary,
        },
    )
    .expect("status allowed");
}

#[test]
fn authorize_denies_with_resource_mapping() {
    let c = caps(&[4], &[4], &[2], false, false);
    // GPIO denials name the pin (§15.16).
    assert_eq!(
        authorize(
            &c,
            entry(GPIO_PIN_READ_ID),
            &ToolArgs::GpioPinRead { pin: pin5() }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(GPIO_PIN_READ_ID),
            resource: 5,
        })
    );
    assert_eq!(
        authorize(
            &c,
            entry(GPIO_PIN_WRITE_ID),
            &ToolArgs::GpioPinWrite {
                pin: pin5(),
                level: Level::Low,
            }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(GPIO_PIN_WRITE_ID),
            resource: 5,
        }),
        "a write outside write_pins fails before dispatch"
    );
    // Sensor denials name the sensor id.
    assert_eq!(
        authorize(
            &c,
            entry(SENSOR_SAMPLE_READ_ID),
            &ToolArgs::SensorSampleRead { sensor: 3 }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(SENSOR_SAMPLE_READ_ID),
            resource: 3,
        })
    );
    // Timer and status denials name resource 0.
    assert_eq!(
        authorize(&c, entry(TIMER_UPTIME_READ_ID), &ToolArgs::TimerUptimeRead),
        Err(Error::PermissionDenied {
            tool: ToolId::new(TIMER_UPTIME_READ_ID),
            resource: 0,
        })
    );
    assert_eq!(
        authorize(
            &c,
            entry(TIMER_DELAY_WAIT_ID),
            &ToolArgs::TimerDelayWait { ms: 10 }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(TIMER_DELAY_WAIT_ID),
            resource: 0,
        })
    );
    assert_eq!(
        authorize(
            &c,
            entry(DEVICE_STATUS_REPORT_ID),
            &ToolArgs::DeviceStatusReport {
                detail: StatusDetail::Full,
            }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(DEVICE_STATUS_REPORT_ID),
            resource: 0,
        })
    );
}

#[test]
fn authorize_denies_sensitive_permissions_by_construction() {
    // No capability set can grant SensitiveWrite: the gate denies it even
    // for a hypothetical entry, keeping AwaitApproval unreachable.
    let mut sensitive = *entry(GPIO_PIN_WRITE_ID);
    sensitive.permission = PermissionClass::SensitiveWrite;
    assert_eq!(
        authorize(
            &full_caps(),
            &sensitive,
            &ToolArgs::GpioPinWrite {
                pin: pin4(),
                level: Level::High,
            }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(GPIO_PIN_WRITE_ID),
            resource: 4,
        })
    );

    let mut irreversible = *entry(TIMER_DELAY_WAIT_ID);
    irreversible.permission = PermissionClass::Irreversible;
    assert_eq!(
        authorize(
            &full_caps(),
            &irreversible,
            &ToolArgs::TimerDelayWait { ms: 10 }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(TIMER_DELAY_WAIT_ID),
            resource: 0,
        })
    );
}

#[test]
fn authorize_denies_args_that_do_not_belong_to_the_tool() {
    // The allowlist pairs each tool id with its own `ToolArgs` shape; a
    // mismatch is a caller bug and fails closed. Unreachable via the
    // decoder, which builds both halves together.
    let c = full_caps();
    assert_eq!(
        authorize(&c, entry(GPIO_PIN_READ_ID), &ToolArgs::TimerUptimeRead),
        Err(Error::PermissionDenied {
            tool: ToolId::new(GPIO_PIN_READ_ID),
            resource: 0,
        })
    );
    assert_eq!(
        authorize(
            &c,
            entry(TIMER_DELAY_WAIT_ID),
            &ToolArgs::GpioPinRead { pin: pin4() }
        ),
        Err(Error::PermissionDenied {
            tool: ToolId::new(TIMER_DELAY_WAIT_ID),
            resource: 4,
        })
    );
}

#[test]
fn timer_and_status_grants_are_independent() {
    let timer_only = caps(&[], &[], &[], true, false);
    authorize(
        &timer_only,
        entry(TIMER_UPTIME_READ_ID),
        &ToolArgs::TimerUptimeRead,
    )
    .expect("timer grant allows uptime");
    assert!(
        authorize(
            &timer_only,
            entry(DEVICE_STATUS_REPORT_ID),
            &ToolArgs::DeviceStatusReport {
                detail: StatusDetail::Summary,
            }
        )
        .is_err(),
        "timer grant does not allow status"
    );

    let status_only = caps(&[], &[], &[], false, true);
    authorize(
        &status_only,
        entry(DEVICE_STATUS_REPORT_ID),
        &ToolArgs::DeviceStatusReport {
            detail: StatusDetail::Summary,
        },
    )
    .expect("status grant allows status");
    assert!(
        authorize(
            &status_only,
            entry(TIMER_DELAY_WAIT_ID),
            &ToolArgs::TimerDelayWait { ms: 10 }
        )
        .is_err(),
        "status grant does not allow timer"
    );
}
