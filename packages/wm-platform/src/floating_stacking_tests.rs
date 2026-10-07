use super::*;

/// Creates native-free observations with stable synthetic identity.
fn entry(
  id: isize,
  owner: Option<isize>,
  enabled: bool,
  visible: bool,
) -> NativeStackingEntry {
  NativeStackingEntry {
    id: WindowId(id),
    owner: owner.map(WindowId),
    enabled,
    visible,
    identity: (100, 101),
  }
}

/// Raises floats back-to-front, including a float already ahead of the
/// tile.
#[test]
fn floating_plan_preserves_mutual_order_and_is_idempotent() {
  for entries in [
    vec![
      entry(1, None, true, true),
      entry(2, None, true, true),
      entry(3, None, true, true),
    ],
    vec![
      entry(2, None, true, true),
      entry(1, None, true, true),
      entry(3, None, true, true),
    ],
  ] {
    assert_eq!(
      floating_raise_plan(
        &entries,
        &[WindowId(2), WindowId(3)],
        &[WindowId(1)]
      )
      .expect("checked graph"),
      vec![WindowId(3), WindowId(2)]
    );
  }
  let ordered = vec![
    entry(2, None, true, true),
    entry(3, None, true, true),
    entry(1, None, true, true),
  ];
  assert_eq!(
    floating_raise_plan(
      &ordered,
      &[WindowId(2), WindowId(3)],
      &[WindowId(1)]
    )
    .expect("checked graph"),
    []
  );
}

/// Retains hidden or disabled owners while allowing safe owned-dialog
/// raises.
#[test]
fn floating_plan_owned_dialog_is_not_blanket_excluded() {
  for visible in [false, true] {
    let entries = vec![
      entry(3, None, true, true),
      entry(2, Some(1), true, true),
      entry(1, None, false, visible),
    ];
    assert_eq!(
      floating_raise_plan(&entries, &[WindowId(2)], &[WindowId(3)])
        .expect("complete retained family"),
      vec![WindowId(2)]
    );
    assert_eq!(
      families(&entries).expect("families")[&WindowId(1)].len(),
      2
    );
  }
}

/// Ownership, modal barriers, and blocked float prefixes outrank
/// preference.
#[test]
fn floating_plan_refuses_owner_child_conflicts_and_modal_crossing() {
  let owner_child =
    vec![entry(1, Some(2), true, true), entry(2, None, true, true)];
  assert_eq!(
    floating_raise_plan(&owner_child, &[WindowId(2)], &[WindowId(1)])
      .expect("checked graph"),
    []
  );
  let modal = vec![
    entry(4, Some(1), true, true),
    entry(1, None, false, true),
    entry(2, None, true, true),
  ];
  assert_eq!(
    floating_raise_plan(&modal, &[WindowId(2)], &[WindowId(1)])
      .expect("checked graph"),
    []
  );
  let blocked = vec![
    entry(1, None, true, true),
    entry(2, None, false, true),
    entry(3, None, true, true),
  ];
  assert_eq!(
    floating_raise_plan(
      &blocked,
      &[WindowId(2), WindowId(3)],
      &[WindowId(1)]
    )
    .expect("checked graph"),
    []
  );
}

/// Invalid ancestry is an error, not an empty successful observation.
#[test]
fn floating_plan_rejects_missing_cyclic_and_disappeared_members() {
  assert!(floating_raise_plan(
    &[entry(2, Some(1), true, true)],
    &[WindowId(2)],
    &[]
  )
  .is_err());
  assert!(floating_raise_plan(
    &[entry(1, Some(2), true, true), entry(2, Some(1), true, true)],
    &[WindowId(2)],
    &[WindowId(1)]
  )
  .is_err());
  assert!(
    floating_raise_plan(&[], &[WindowId(2)], &[WindowId(1)]).is_err()
  );
}
