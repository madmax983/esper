//! Static catalog, capabilities, and the policy gate (§5).

use esper_core::ids::{Pin, ToolId};
use esper_core::registry::{
    authorize_capability, lookup_by_id, lookup_by_name, Capabilities, IdempotencyStrategy,
    PermissionClass, ToolEntry, VerificationStrategy, CATALOG, GPIO_PIN_READ_ID,
    GPIO_PIN_READ_NAME, GPIO_PIN_WRITE_ID, GPIO_PIN_WRITE_NAME, RESULT_BOUND_BYTES,
};
use esper_core::Error;

fn caps(read: &[u8], write: &[u8]) -> Capabilities {
    let mut c = Capabilities::empty();
    for (i, pin) in read.iter().enumerate() {
        c.read_pins[i] = *pin;
    }
    c.read_count = u8::try_from(read.len()).expect("test pins fit in u8");
    for (i, pin) in write.iter().enumerate() {
        c.write_pins[i] = *pin;
    }
    c.write_count = u8::try_from(write.len()).expect("test pins fit in u8");
    c
}

#[test]
fn catalog_holds_exactly_the_two_slice_tools() {
    assert_eq!(CATALOG.len(), 2);

    let read = &CATALOG[0];
    assert_eq!(read.id, ToolId::new(GPIO_PIN_READ_ID));
    assert_eq!(read.id.get(), 1);
    assert_eq!(read.name, GPIO_PIN_READ_NAME);
    assert_eq!(read.permission, PermissionClass::ReadOnly);
    assert_eq!(read.verification, VerificationStrategy::None);
    assert_eq!(read.idempotency, IdempotencyStrategy::NotApplicable);
    assert_eq!(read.result_bound, RESULT_BOUND_BYTES);

    let write = &CATALOG[1];
    assert_eq!(write.id, ToolId::new(GPIO_PIN_WRITE_ID));
    assert_eq!(write.id.get(), 2);
    assert_eq!(write.name, GPIO_PIN_WRITE_NAME);
    assert_eq!(write.permission, PermissionClass::IdempotentWrite);
    assert_eq!(write.verification, VerificationStrategy::ReadBack);
    assert_eq!(write.idempotency, IdempotencyStrategy::SetOperation);
    assert_eq!(write.result_bound, RESULT_BOUND_BYTES);
}

#[test]
fn no_slice_tool_carries_a_sensitive_permission() {
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
    let write = lookup_by_id(ToolId::new(2)).expect("tool 2");
    assert_eq!(write.name, "gpio_pin_write");
    assert!(lookup_by_id(ToolId::new(3)).is_none());

    assert_eq!(
        lookup_by_name(b"gpio_pin_read").expect("by name").id,
        ToolId::new(1)
    );
    assert_eq!(
        lookup_by_name(b"gpio_pin_write").expect("by name").id,
        ToolId::new(2)
    );
    assert!(lookup_by_name(b"gpio_frobnicator").is_none());
    assert!(lookup_by_name(b"").is_none());
}

#[test]
fn capabilities_gate_reads_and_writes() {
    let c = caps(&[4], &[4]);
    assert!(c.can_read(Pin::new(4).expect("pin")));
    assert!(!c.can_read(Pin::new(5).expect("pin")));
    assert!(c.can_write(Pin::new(4).expect("pin")));
    assert!(!c.can_write(Pin::new(5).expect("pin")));
}

#[test]
fn empty_capabilities_deny_everything() {
    let c = Capabilities::empty();
    for pin in 0..8u8 {
        let pin = Pin::new(pin).expect("pin");
        assert!(!c.can_read(pin));
        assert!(!c.can_write(pin));
    }
}

#[test]
fn counts_beyond_storage_fail_closed() {
    let mut c = caps(&[4], &[]);
    c.read_count = 9; // claims more pins than storage holds
    assert!(!c.can_read(Pin::new(4).expect("pin")));
}

#[test]
fn widens_detects_added_pins() {
    let seed = caps(&[4], &[4]);
    assert!(!caps(&[4], &[4]).widens(&seed));
    assert!(!caps(&[], &[4]).widens(&seed), "shrinking is not widening");
    assert!(!caps(&[], &[]).widens(&seed));

    let mut wider = caps(&[4], &[4]);
    wider.read_pins[1] = 5;
    wider.read_count = 2;
    assert!(wider.widens(&seed), "added read pin widens");

    let mut wider_write = caps(&[4], &[4]);
    wider_write.write_pins[1] = 6;
    wider_write.write_count = 2;
    assert!(wider_write.widens(&seed), "added write pin widens");
}

#[test]
fn authorize_capability_allows_granted_pins() {
    let c = caps(&[4], &[4]);
    let read = lookup_by_id(ToolId::new(1)).expect("read");
    let write = lookup_by_id(ToolId::new(2)).expect("write");
    let pin = Pin::new(4).expect("pin");
    authorize_capability(&c, read, pin).expect("read allowed");
    authorize_capability(&c, write, pin).expect("write allowed");
}

#[test]
fn authorize_capability_denies_ungranted_pins() {
    let c = caps(&[4], &[4]);
    let read = lookup_by_id(ToolId::new(1)).expect("read");
    let write = lookup_by_id(ToolId::new(2)).expect("write");
    let pin5 = Pin::new(5).expect("pin");
    assert_eq!(
        authorize_capability(&c, read, pin5),
        Err(Error::PermissionDenied {
            tool: ToolId::new(1),
            pin: 5
        })
    );
    assert_eq!(
        authorize_capability(&c, write, pin5),
        Err(Error::PermissionDenied {
            tool: ToolId::new(2),
            pin: 5
        }),
        "a write outside write_pins fails before dispatch"
    );
}

#[test]
fn authorize_capability_denies_sensitive_permissions_by_construction() {
    // No capability set can grant SensitiveWrite: the gate denies it even
    // for a hypothetical entry, keeping AwaitApproval unreachable.
    let sensitive = ToolEntry {
        permission: PermissionClass::SensitiveWrite,
        ..CATALOG[1]
    };
    let c = caps(&[4], &[4]);
    assert_eq!(
        authorize_capability(&c, &sensitive, Pin::new(4).expect("pin")),
        Err(Error::PermissionDenied {
            tool: ToolId::new(2),
            pin: 4
        })
    );
}
