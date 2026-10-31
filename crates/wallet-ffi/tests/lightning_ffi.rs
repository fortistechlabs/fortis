#![cfg(feature = "lightning")]

use fortis_lightning::ldk_node::bitcoin::Network;
use fortis_lightning::{
    Balances, ChannelState, Direction, LnError, LnEvent, LspLimits, ParseError, Payment,
    PaymentTarget, ReceiveQuote, StartMode, Status,
};
use wallet_ffi::{
    ln_bip39_seed, ln_estimate_fee_msat, ln_parse, ln_parse_at, LnBalances, LnChannelState,
    LnDirection, LnEventRecord, LnFfiError, LnLspLimits, LnPayment, LnPaymentStatus,
    LnReceiveQuote, LnStartMode, LnTarget, LnUnifiedLightning,
};

const B11_25M: &str = "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu";
const OFFER: &str = "lno1pqps7sjqpgtyzm3qv4uxzmtsd3jjqer9wd3hy6tsw35k7msjzfpy7nz5yqcnygrfdej82um5wf5k2uckyypwa3eyt44h6txtxquqh7lz5djge4afgfjn7k4rgrkuag0jsd5xvxg";
const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
const NOW_VALID: u64 = 1_496_314_668; // 10 seconds after B11_25M timestamp (1496314658)

#[test]
fn parse_roundtrip_through_ffi_types() {
    let inputs = [
        B11_25M.to_string(),
        OFFER.to_string(),
        "lnurlp://example.com/.well-known/lnurlp/alice".to_string(),
        "alice@example.com".to_string(),
        ADDR.to_string(),
        format!("bitcoin:{ADDR}?amount=0.025&lightning={B11_25M}"),
        format!("bitcoin:{ADDR}?amount=0.001&lno={OFFER}"),
    ];

    for input in inputs {
        let expected = fortis_lightning::parse_at(&input, Network::Bitcoin, NOW_VALID).unwrap();
        let ffi_target = ln_parse_at(input.clone(), "bitcoin".into(), NOW_VALID).unwrap();
        let roundtripped = PaymentTarget::from(ffi_target.clone());
        assert_eq!(roundtripped, expected, "roundtrip mismatch for {input}");
        let ffi_again = LnTarget::try_from(roundtripped).unwrap();
        assert_eq!(ffi_again, ffi_target);
    }

    // `ln_parse` works for non-expiring targets (address, offer, lnurl, lightning address)
    assert!(matches!(
        ln_parse(ADDR.into(), "mainnet".into()).unwrap(),
        LnTarget::OnChain { ref address, amount_sat: None } if address == ADDR
    ));
    assert!(matches!(
        ln_parse("alice@example.com".into(), "bitcoin".into()).unwrap(),
        LnTarget::LightningAddress { ref user, ref domain } if user == "alice" && domain == "example.com"
    ));

    // Expired BOLT11 via `ln_parse` returns `InvoiceExpired`
    assert_eq!(
        ln_parse(B11_25M.into(), "bitcoin".into()),
        Err(LnFfiError::InvoiceExpired)
    );

    // Wrong network returns `WrongNetwork`
    assert_eq!(
        ln_parse_at(B11_25M.into(), "regtest".into(), NOW_VALID),
        Err(LnFfiError::WrongNetwork)
    );
}

#[test]
fn error_variants_map_one_to_one() {
    let all_core_errors = [
        LnError::BackupUnavailable,
        LnError::ActiveOnAnotherDevice,
        LnError::LspUnreachable,
        LnError::InsufficientLightning,
        LnError::NoRoute,
        LnError::InvoiceExpired,
        LnError::WrongNetwork,
        LnError::PaymentStateUnknown,
        LnError::AmountOutOfRange {
            min_msat: 1_000_000,
            max_msat: 500_000_000,
        },
        LnError::Other("custom detail".into()),
    ];

    for core_err in all_core_errors {
        let ffi_err = LnFfiError::from(core_err.clone());
        match (&core_err, &ffi_err) {
            (LnError::BackupUnavailable, LnFfiError::BackupUnavailable)
            | (LnError::ActiveOnAnotherDevice, LnFfiError::ActiveOnAnotherDevice)
            | (LnError::LspUnreachable, LnFfiError::LspUnreachable)
            | (LnError::InsufficientLightning, LnFfiError::InsufficientLightning)
            | (LnError::NoRoute, LnFfiError::NoRoute)
            | (LnError::InvoiceExpired, LnFfiError::InvoiceExpired)
            | (LnError::WrongNetwork, LnFfiError::WrongNetwork)
            | (LnError::PaymentStateUnknown, LnFfiError::PaymentStateUnknown) => {}
            (
                LnError::AmountOutOfRange {
                    min_msat: a,
                    max_msat: b,
                },
                LnFfiError::AmountOutOfRange {
                    min_msat: c,
                    max_msat: d,
                },
            ) => {
                assert_eq!((a, b), (c, d));
            }
            (LnError::Other(a), LnFfiError::Other { reason: b }) => {
                assert_eq!(a, b);
            }
            _ => panic!("mismatched error mapping: {core_err:?} -> {ffi_err:?}"),
        }
        let back = LnError::from(ffi_err);
        assert_eq!(back, core_err);
    }

    // ParseError mappings
    assert_eq!(
        LnFfiError::from(ParseError::WrongNetwork),
        LnFfiError::WrongNetwork
    );
    assert_eq!(
        LnFfiError::from(ParseError::Expired),
        LnFfiError::InvoiceExpired
    );
    assert!(matches!(
        LnFfiError::from(ParseError::Empty),
        LnFfiError::Other { .. }
    ));
    assert!(matches!(
        LnFfiError::from(ParseError::Unrecognised),
        LnFfiError::Other { .. }
    ));
    assert!(matches!(
        LnFfiError::from(ParseError::Invalid("bad".into())),
        LnFfiError::Other { .. }
    ));
}

#[test]
fn record_and_enum_conversions_roundtrip() {
    let b = Balances {
        lightning_sat: 100,
        onchain_transit_sat: 200,
        pending_close_sat: 300,
        reserved_sat: 400,
        pending_transit_sat: 500,
        inbound_sat: 600,
    };
    assert_eq!(
        LnBalances::from(b),
        LnBalances {
            lightning_sat: 100,
            onchain_transit_sat: 200,
            pending_close_sat: 300,
            reserved_sat: 400,
            pending_transit_sat: 500,
            inbound_sat: 600,
        }
    );

    assert_eq!(LnChannelState::from(ChannelState::None), LnChannelState::None);
    assert_eq!(
        LnChannelState::from(ChannelState::Opening),
        LnChannelState::Opening
    );
    assert_eq!(LnChannelState::from(ChannelState::Ready), LnChannelState::Ready);
    assert_eq!(
        LnChannelState::from(ChannelState::Closing),
        LnChannelState::Closing
    );

    assert_eq!(StartMode::from(LnStartMode::Normal), StartMode::Normal);
    assert_eq!(StartMode::from(LnStartMode::Exit), StartMode::Exit);

    let limits = LspLimits {
        min_payment_msat: 10_000_000,
        max_payment_msat: 500_000_000,
        channel_min_sat: 100_000,
        channel_max_sat: 500_000,
    };
    assert_eq!(LspLimits::from(LnLspLimits::from(limits)), limits);

    let quote = ReceiveQuote {
        invoice: "lnbc1...".into(),
        amount_msat: 50_000_000,
        fee_msat: 500_000,
        needs_channel: true,
    };
    assert_eq!(
        LnReceiveQuote::from(quote),
        LnReceiveQuote {
            invoice: "lnbc1...".into(),
            amount_msat: 50_000_000,
            fee_msat: 500_000,
            needs_channel: true,
        }
    );

    let p = Payment {
        id: "abc".into(),
        direction: Direction::Out,
        amount_msat: 10_000,
        fee_msat: 100,
        status: Status::Succeeded,
        description: "coffee".into(),
        timestamp: 1_700_000_000,
    };
    assert_eq!(
        LnPayment::from(p),
        LnPayment {
            id: "abc".into(),
            direction: LnDirection::Out,
            amount_msat: 10_000,
            fee_msat: 100,
            status: LnPaymentStatus::Succeeded,
            description: "coffee".into(),
            timestamp: 1_700_000_000,
        }
    );

    assert_eq!(
        LnEventRecord::from(LnEvent::DeviceLockLost),
        LnEventRecord::DeviceLockLost
    );
    assert_eq!(
        LnEventRecord::from(LnEvent::ChannelReady),
        LnEventRecord::ChannelReady
    );

    // Fee helper and BIP-39 seed helper
    let t = LnTarget::Unified {
        address: ADDR.into(),
        amount_sat: Some(1_000),
        lightning: LnUnifiedLightning::Bolt11 {
            invoice: B11_25M.into(),
            amount_msat: Some(100_000_000),
            description: "coffee".into(),
            expiry_secs: 3600,
        },
    };
    assert_eq!(ln_estimate_fee_msat(t, 0), 1_050_000);

    let seed = ln_bip39_seed(
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".into(),
        "".into(),
    )
    .unwrap();
    assert_eq!(seed.len(), 64);
}
