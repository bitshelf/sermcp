//! Before/after device-set diff on PARSED identities (never raw stdout):
//! 0 unmatched → keep polling; 1 → this DUT's device; >1 → ambiguous
//! (never take the first — that is how the wrong board gets flashed).

/// A device identity: `Stable` for a vendor serial/SID, `Composite` for
/// a field tuple when no single stable key exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceIdentity {
    Stable(String),
    Composite(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceDiff {
    NoNew,
    Unique(super::provider::DownloadDevice),
    Ambiguous(Vec<super::provider::DownloadDevice>),
}

/// Multiset diff of `after` against `before` on identity keys: every
/// `after` record whose identity is not consumed by a `before` record.
/// Multiple same-identity devices are counted, never set-collapsed.
pub fn diff_devices(
    before: &[super::provider::DownloadDevice],
    after: &[super::provider::DownloadDevice],
) -> DeviceDiff {
    use std::collections::BTreeMap;
    let mut baseline: BTreeMap<&DeviceIdentity, usize> = BTreeMap::new();
    for device in before {
        *baseline.entry(&device.identity).or_default() += 1;
    }
    let mut added = Vec::new();
    for device in after {
        let count = baseline.get_mut(&device.identity);
        match count {
            Some(n) if *n > 0 => *n -= 1,
            _ => added.push(device.clone()),
        }
    }
    match added.len() {
        0 => DeviceDiff::NoNew,
        1 => DeviceDiff::Unique(added.remove(0)),
        _ => DeviceDiff::Ambiguous(added),
    }
}

#[cfg(test)]
mod tests {
    use super::super::provider::DownloadDevice;
    use super::*;

    fn dev(raw: &str, identity: DeviceIdentity, summary: &str) -> DownloadDevice {
        DownloadDevice {
            raw: raw.into(),
            identity,
            summary: summary.into(),
        }
    }

    #[test]
    fn unique_new_device_and_header_noise_never_counted() {
        let before = vec![dev(
            "DevNo=0 SerialNo=AAA",
            DeviceIdentity::Stable("AAA".into()),
            "AAA",
        )];
        let after = vec![
            dev(
                "DevNo=0 SerialNo=AAA",
                DeviceIdentity::Stable("AAA".into()),
                "AAA",
            ),
            dev(
                "DevNo=1 SerialNo=BBB",
                DeviceIdentity::Stable("BBB".into()),
                "BBB",
            ),
        ];
        match diff_devices(&before, &after) {
            DeviceDiff::Unique(d) => assert_eq!(d.identity, DeviceIdentity::Stable("BBB".into())),
            other => panic!("expected unique, got {other:?}"),
        }
        // Counted header growth (connected(0) → connected(1)) never even
        // reaches the diff: parse_devices drops non-device lines.
        assert_eq!(diff_devices(&before, &before), DeviceDiff::NoNew);
    }

    #[test]
    fn ambiguous_when_multiple_new_devices() {
        let before = vec![
            dev("A", DeviceIdentity::Stable("A".into()), "A"),
            dev("B", DeviceIdentity::Stable("B".into()), "B"),
        ];
        let after = vec![
            dev("A", DeviceIdentity::Stable("A".into()), "A"),
            dev("B", DeviceIdentity::Stable("B".into()), "B"),
            dev("C", DeviceIdentity::Stable("C".into()), "C"),
            dev("D", DeviceIdentity::Stable("D".into()), "D"),
        ];
        match diff_devices(&before, &after) {
            DeviceDiff::Ambiguous(devices) => {
                assert_eq!(devices.len(), 2);
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn multiset_same_identity_counts_not_set_membership() {
        // A lab wall of identical devices: same Composite identity, count
        // grows 3 → 4, so exactly the one new record is the addition.
        let composite = || DeviceIdentity::Composite(vec!["soc-t527".into(), "fel".into()]);
        let before: Vec<_> = (0..3)
            .map(|i| dev(&format!("r{i}"), composite(), "t527"))
            .collect();
        let mut after = before.clone();
        after.push(dev("r3", composite(), "t527"));
        assert!(matches!(
            diff_devices(&before, &after),
            DeviceDiff::Unique(_)
        ));
        let fewer: Vec<_> = after[..3].to_vec();
        assert_eq!(diff_devices(&after, &fewer), DeviceDiff::NoNew);
    }
}
