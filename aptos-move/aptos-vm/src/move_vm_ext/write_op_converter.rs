// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::move_vm_ext::{session::BytesWithResourceLayout, AptosMoveResolver};
use aptos_aggregator::delta_change_set::serialize;
use aptos_types::{
    on_chain_config::{CurrentTimeMicroseconds, OnChainConfig},
    state_store::{
        state_key::StateKey,
        state_value::{StateValueMetadata, StateValueMetadataKind},
    },
    write_set::WriteOp,
};
use aptos_vm_types::change_set::GroupWrite;
use bytes::Bytes;
use move_core_types::{
    effects::Op as MoveStorageOp,
    language_storage::StructTag,
    value::MoveTypeLayout,
    vm_status::{err_msg, StatusCode, VMStatus},
};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) struct WriteOpConverter<'r> {
    remote: &'r dyn AptosMoveResolver,
    new_slot_metadata: Option<StateValueMetadata>,
}

macro_rules! convert_impl {
    ($convert_func_name:ident, $get_metadata_callback:ident) => {
        pub(crate) fn $convert_func_name(
            &self,
            state_key: &StateKey,
            move_storage_op: MoveStorageOp<Bytes>,
            legacy_creation_as_modification: bool,
        ) -> Result<WriteOp, VMStatus> {
            let move_storage_op = match move_storage_op {
                MoveStorageOp::New(data) => MoveStorageOp::New((data, None)),
                MoveStorageOp::Modify(data) => MoveStorageOp::Modify((data, None)),
                MoveStorageOp::Delete => MoveStorageOp::Delete,
            };
            self.convert(
                self.remote.$get_metadata_callback(state_key),
                move_storage_op,
                legacy_creation_as_modification,
            )
        }
    };
}

impl<'r> WriteOpConverter<'r> {
    convert_impl!(convert_module, get_module_state_value_metadata);

    convert_impl!(convert_aggregator, get_aggregator_v1_state_value_metadata);

    pub(crate) fn new(
        remote: &'r dyn AptosMoveResolver,
        is_storage_slot_metadata_enabled: bool,
    ) -> Self {
        let mut new_slot_metadata: Option<StateValueMetadata> = None;
        if is_storage_slot_metadata_enabled {
            if let Some(current_time) = CurrentTimeMicroseconds::fetch_config(remote) {
                // The deposit on the metadata is a placeholder (0), it will be updated later when
                // storage fee is charged.
                new_slot_metadata = Some(StateValueMetadata::new(0, &current_time));
            }
        }

        Self {
            remote,
            new_slot_metadata,
        }
    }

    pub(crate) fn convert_resource(
        &self,
        state_key: &StateKey,
        move_storage_op: MoveStorageOp<BytesWithResourceLayout>,
        legacy_creation_as_modification: bool,
    ) -> Result<(WriteOp, Option<Arc<MoveTypeLayout>>), VMStatus> {
        let result = self.convert(
            self.remote.get_resource_state_value_metadata(state_key),
            move_storage_op.clone(),
            legacy_creation_as_modification,
        );
        match move_storage_op {
            MoveStorageOp::New((_, type_layout)) => Ok((result?, type_layout)),
            MoveStorageOp::Modify((_, type_layout)) => Ok((result?, type_layout)),
            MoveStorageOp::Delete => Ok((result?, None)),
        }
    }

    pub(crate) fn convert_resource_group_v1(
        &self,
        state_key: &StateKey,
        group_changes: BTreeMap<StructTag, MoveStorageOp<BytesWithResourceLayout>>,
    ) -> Result<GroupWrite, VMStatus> {
        // Resource group metadata is stored at the group StateKey, and can be obtained via the
        // same interfaces at for a resource at a given StateKey.
        let state_value_metadata_result = self.remote.get_resource_state_value_metadata(state_key);
        // Currently, due to read-before-write and a gas charge on the first read that is based
        // on the group size, this should simply re-read a cached (speculative) group size.
        let pre_group_size = self.remote.resource_group_size(state_key).map_err(|_| {
            VMStatus::error(
                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                err_msg("Error querying resource group size"),
            )
        })?;

        let mut inner_ops = BTreeMap::new();

        // We set SPECULATIVE_EXECUTION_ABORT_ERROR here, as the error can happen due to
        // speculative reads (and in a non-speculative context, e.g. during commit, it
        // is a more serious error and block execution must abort).
        // BlockExecutor is responsible with handling this error.
        let group_size_arithmetics_error = || {
            VMStatus::error(
                StatusCode::SPECULATIVE_EXECUTION_ABORT_ERROR,
                err_msg("Group size underflow while applying updates"),
            )
        };
        let tag_serialization_error = |_| {
            VMStatus::error(
                StatusCode::VALUE_SERIALIZATION_ERROR,
                err_msg("Tag serialization error"),
            )
        };
        let post_group_size =
            group_changes
                .into_iter()
                .try_fold(pre_group_size, |cur_size, (tag, current_op)| {
                    let tag_size =
                        bcs::serialized_size(&tag).map_err(tag_serialization_error)? as u64;

                    // We go over the resources in the group change-set, query their previous size,
                    // and subtract those from the speculative group size prior to the transaction
                    // (then, we add the new sizes from the change-set). The reason we do not instead
                    // get and add the sizes of the resources in the group but not in the change-set
                    // is to avoid creating unnecessary R/W conflicts (the resources in the change-set
                    // are already read, but the other resources are not).
                    let cur_size = if !matches!(current_op, MoveStorageOp::New(_)) {
                        let old_size = self
                            .remote
                            .resource_size_in_group(state_key, &tag)
                            .map_err(|_| {
                                VMStatus::error(
                                    StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                                    err_msg("Error querying resource group size"),
                                )
                            })?
                            + tag_size;
                        cur_size
                            .checked_sub(old_size)
                            .ok_or_else(group_size_arithmetics_error)?
                    } else {
                        cur_size
                    };

                    let (new_size, legacy_op) = match current_op {
                        MoveStorageOp::Delete => (cur_size, (WriteOp::Deletion, None)),
                        MoveStorageOp::Modify((new_data, maybe_layout)) => (
                            cur_size
                                .checked_add(new_data.len() as u64 + tag_size)
                                .ok_or_else(group_size_arithmetics_error)?,
                            (WriteOp::Modification(new_data), maybe_layout),
                        ),
                        MoveStorageOp::New((data, maybe_layout)) => (
                            cur_size
                                .checked_add(data.len() as u64 + tag_size)
                                .ok_or_else(group_size_arithmetics_error)?,
                            (WriteOp::Creation(data), maybe_layout),
                        ),
                    };
                    inner_ops.insert(tag, legacy_op);
                    Ok::<u64, VMStatus>(new_size)
                })?;

        // Create the op that would look like a combined V0 resource group MoveStorageOp,
        // except it encodes the (speculative) size of the group after applying the updates
        // which is used for charging storage fees. Moreover, the metadata computation occurs
        // fully backwards compatibly, and lets obtain final storage op by replacing bytes.
        // TODO[agg_v2](fix) fix layout for RG
        let metadata_op = if post_group_size == 0 {
            MoveStorageOp::Delete
        } else if pre_group_size == 0 {
            MoveStorageOp::New((Bytes::new(), None))
        } else {
            MoveStorageOp::Modify((Bytes::new(), None))
        };
        Ok(GroupWrite::new(
            self.convert(state_value_metadata_result, metadata_op, false)?,
            post_group_size,
            inner_ops,
        ))
    }

    fn convert(
        &self,
        state_value_metadata_result: anyhow::Result<Option<StateValueMetadataKind>>,
        move_storage_op: MoveStorageOp<BytesWithResourceLayout>,
        legacy_creation_as_modification: bool,
    ) -> Result<WriteOp, VMStatus> {
        use MoveStorageOp::*;
        use WriteOp::*;

        let maybe_existing_metadata = state_value_metadata_result.map_err(|_| {
            VMStatus::error(
                StatusCode::STORAGE_ERROR,
                err_msg("Storage read failed when converting change set."),
            )
        })?;

        let write_op = match (maybe_existing_metadata, move_storage_op) {
            (None, Modify(_) | Delete) => {
                return Err(VMStatus::error(
                    // Possible under speculative execution, returning speculative error waiting for re-execution
                    StatusCode::SPECULATIVE_EXECUTION_ABORT_ERROR,
                    err_msg("When converting write op: updating non-existent value."),
                ));
            },
            (Some(_), New(_)) => {
                return Err(VMStatus::error(
                    // Possible under speculative execution, returning speculative error waiting for re-execution
                    StatusCode::SPECULATIVE_EXECUTION_ABORT_ERROR,
                    err_msg("When converting write op: Recreating existing value."),
                ));
            },
            (None, New((data, _))) => match &self.new_slot_metadata {
                None => {
                    if legacy_creation_as_modification {
                        Modification(data)
                    } else {
                        Creation(data)
                    }
                },
                Some(metadata) => CreationWithMetadata {
                    data,
                    metadata: metadata.clone(),
                },
            },
            (Some(existing_metadata), Modify((data, _))) => {
                // Inherit metadata even if the feature flags is turned off, for compatibility.
                match existing_metadata {
                    None => Modification(data),
                    Some(metadata) => ModificationWithMetadata { data, metadata },
                }
            },
            (Some(existing_metadata), Delete) => {
                // Inherit metadata even if the feature flags is turned off, for compatibility.
                match existing_metadata {
                    None => Deletion,
                    Some(metadata) => DeletionWithMetadata { metadata },
                }
            },
        };
        Ok(write_op)
    }

    pub(crate) fn convert_aggregator_modification(
        &self,
        state_key: &StateKey,
        value: u128,
    ) -> Result<WriteOp, VMStatus> {
        let maybe_existing_metadata = self
            .remote
            .get_aggregator_v1_state_value_metadata(state_key)
            .map_err(|_| VMStatus::error(StatusCode::SPECULATIVE_EXECUTION_ABORT_ERROR, None))?;
        let data = serialize(&value).into();

        let op = match maybe_existing_metadata {
            None => {
                match &self.new_slot_metadata {
                    // n.b. Aggregator writes historically did not distinguish Create vs Modify.
                    None => WriteOp::Modification(data),
                    Some(metadata) => WriteOp::CreationWithMetadata {
                        data,
                        metadata: metadata.clone(),
                    },
                }
            },
            Some(existing_metadata) => match existing_metadata {
                None => WriteOp::Modification(data),
                Some(metadata) => WriteOp::ModificationWithMetadata { data, metadata },
            },
        };

        Ok(op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        data_cache::tests::as_resolver_with_group_size_kind,
        move_vm_ext::resolver::ResourceGroupResolver,
    };
    use aptos_state_view::TStateView;
    use aptos_types::{
        account_address::AccountAddress,
        state_store::{state_storage_usage::StateStorageUsage, state_value::StateValue},
    };
    use aptos_vm_types::resource_group_adapter::GroupSizeKind;
    use claims::{assert_none, assert_some_eq};
    use move_core_types::{
        identifier::Identifier,
        language_storage::{StructTag, TypeTag},
    };

    fn raw_metadata(v: u64) -> StateValueMetadata {
        StateValueMetadata::new(v, &CurrentTimeMicroseconds { microseconds: v })
    }

    // TODO: Can re-use some of these testing definitions with aptos-vm-types.
    pub(crate) fn mock_tag_0() -> StructTag {
        StructTag {
            address: AccountAddress::ONE,
            module: Identifier::new("a").unwrap(),
            name: Identifier::new("a").unwrap(),
            type_params: vec![TypeTag::U8],
        }
    }

    pub(crate) fn mock_tag_1() -> StructTag {
        StructTag {
            address: AccountAddress::ONE,
            module: Identifier::new("abcde").unwrap(),
            name: Identifier::new("fgh").unwrap(),
            type_params: vec![TypeTag::U64],
        }
    }

    pub(crate) fn mock_tag_2() -> StructTag {
        StructTag {
            address: AccountAddress::ONE,
            module: Identifier::new("abcdex").unwrap(),
            name: Identifier::new("fghx").unwrap(),
            type_params: vec![TypeTag::U128],
        }
    }

    struct MockStateView {
        data: BTreeMap<StateKey, StateValue>,
    }

    impl MockStateView {
        fn new(data: BTreeMap<StateKey, StateValue>) -> Self {
            Self { data }
        }
    }

    impl TStateView for MockStateView {
        type Key = StateKey;

        fn get_state_value(&self, state_key: &Self::Key) -> anyhow::Result<Option<StateValue>> {
            Ok(self.data.get(state_key).cloned())
        }

        fn get_usage(&self) -> anyhow::Result<StateStorageUsage> {
            unimplemented!();
        }
    }

    // TODO[agg_v2](fix) make as_resolver_with_group_size_kind support AsSum
    // #[test]
    #[allow(unused)]
    fn size_computation_delete_modify_ops() {
        let group: BTreeMap<StructTag, Bytes> = BTreeMap::from([
            (mock_tag_0(), vec![1].into()),
            (mock_tag_1(), vec![2, 2].into()),
            (mock_tag_2(), vec![3, 3, 3].into()),
        ]);
        let metadata = raw_metadata(100);
        let key = StateKey::raw(vec![0]);

        let data = BTreeMap::from([(
            key.clone(),
            StateValue::new_with_metadata(bcs::to_bytes(&group).unwrap().into(), metadata.clone()),
        )]);

        let expected_size = bcs::serialized_size(&mock_tag_0()).unwrap()
            + bcs::serialized_size(&mock_tag_1()).unwrap()
            + bcs::serialized_size(&mock_tag_2()).unwrap()
            + 6; // values bytes size: 1 + 2 + 3.

        let s = MockStateView::new(data);
        let resolver = as_resolver_with_group_size_kind(&s, GroupSizeKind::AsSum);

        assert_eq!(
            resolver.resource_group_size(&key).unwrap(),
            expected_size as u64
        );
        // TODO: Layout hardcoded to None. Test with layout = Some(..)
        let group_changes = BTreeMap::from([
            (mock_tag_0(), MoveStorageOp::Delete),
            (
                mock_tag_2(),
                MoveStorageOp::Modify((vec![5, 5, 5, 5, 5].into(), None)),
            ),
        ]);
        let converter = WriteOpConverter::new(&resolver, false);
        let group_write = converter
            .convert_resource_group_v1(&key, group_changes)
            .unwrap();

        assert_eq!(group_write.metadata_op().metadata(), Some(&metadata));
        let expected_new_size = bcs::serialized_size(&mock_tag_1()).unwrap()
            + bcs::serialized_size(&mock_tag_2()).unwrap()
            + 7; // values bytes size: 2 + 5
        assert_eq!(
            bcs::from_bytes::<u64>(group_write.metadata_op().bytes().unwrap()).unwrap(),
            expected_new_size as u64
        );
        assert_eq!(group_write.inner_ops().len(), 2);
        assert_some_eq!(
            group_write.inner_ops().get(&mock_tag_0()),
            &(WriteOp::Deletion, None)
        );
        assert_some_eq!(
            group_write.inner_ops().get(&mock_tag_2()),
            &(WriteOp::Modification(vec![5, 5, 5, 5, 5].into()), None)
        );
    }

    // TODO[agg_v2](fix) make as_resolver_with_group_size_kind support AsSum
    // #[test]
    #[allow(unused)]
    fn size_computation_new_op() {
        let group: BTreeMap<StructTag, Bytes> = BTreeMap::from([
            (mock_tag_0(), vec![1].into()),
            (mock_tag_1(), vec![2, 2].into()),
        ]);
        let metadata = raw_metadata(100);
        let key = StateKey::raw(vec![0]);

        let data = BTreeMap::from([(
            key.clone(),
            StateValue::new_with_metadata(bcs::to_bytes(&group).unwrap().into(), metadata.clone()),
        )]);

        let s = MockStateView::new(data);
        let resolver = as_resolver_with_group_size_kind(&s, GroupSizeKind::AsSum);

        let group_changes = BTreeMap::from([(
            mock_tag_2(),
            MoveStorageOp::New((vec![3, 3, 3].into(), None)),
        )]);
        let converter = WriteOpConverter::new(&resolver, true);
        let group_write = converter
            .convert_resource_group_v1(&key, group_changes)
            .unwrap();

        assert_eq!(group_write.metadata_op().metadata(), Some(&metadata));
        let expected_new_size = bcs::serialized_size(&mock_tag_0()).unwrap()
            + bcs::serialized_size(&mock_tag_1()).unwrap()
            + bcs::serialized_size(&mock_tag_2()).unwrap()
            + 6; // values bytes size: 1 + 2 + 3.
        assert_eq!(
            bcs::from_bytes::<u64>(group_write.metadata_op().bytes().unwrap()).unwrap(),
            expected_new_size as u64
        );
        assert_eq!(group_write.inner_ops().len(), 1);
        assert_some_eq!(
            group_write.inner_ops().get(&mock_tag_2()),
            &(WriteOp::Creation(vec![3, 3, 3].into()), None)
        );
    }

    // TODO[agg_v2](fix) make as_resolver_with_group_size_kind support AsSum
    // #[test]
    #[allow(unused)]
    fn size_computation_new_group() {
        let s = MockStateView::new(BTreeMap::new());
        let resolver = as_resolver_with_group_size_kind(&s, GroupSizeKind::AsSum);

        // TODO: Layout hardcoded to None. Test with layout = Some(..)
        let group_changes =
            BTreeMap::from([(mock_tag_1(), MoveStorageOp::New((vec![2, 2].into(), None)))]);
        let key = StateKey::raw(vec![0]);
        let converter = WriteOpConverter::new(&resolver, true);
        let group_write = converter
            .convert_resource_group_v1(&key, group_changes)
            .unwrap();

        assert_none!(group_write.metadata_op().metadata());
        let expected_new_size = bcs::serialized_size(&mock_tag_1()).unwrap() + 2;
        assert_eq!(
            bcs::from_bytes::<u64>(group_write.metadata_op().bytes().unwrap()).unwrap(),
            expected_new_size as u64
        );
        assert_eq!(group_write.inner_ops().len(), 1);
        assert_some_eq!(
            group_write.inner_ops().get(&mock_tag_1()),
            &(WriteOp::Creation(vec![2, 2].into()), None)
        );
    }

    // TODO[agg_v2](fix) make as_resolver_with_group_size_kind support AsSum
    // #[test]
    #[allow(unused)]
    fn size_computation_delete_group() {
        let group: BTreeMap<StructTag, Bytes> = BTreeMap::from([
            (mock_tag_0(), vec![1].into()),
            (mock_tag_1(), vec![2, 2].into()),
        ]);
        let metadata = raw_metadata(100);
        let key = StateKey::raw(vec![0]);

        let data = BTreeMap::from([(
            key.clone(),
            StateValue::new_with_metadata(bcs::to_bytes(&group).unwrap().into(), metadata.clone()),
        )]);

        let s = MockStateView::new(data);
        let resolver = as_resolver_with_group_size_kind(&s, GroupSizeKind::AsSum);
        let group_changes = BTreeMap::from([
            (mock_tag_0(), MoveStorageOp::Delete),
            (mock_tag_1(), MoveStorageOp::Delete),
        ]);
        let converter = WriteOpConverter::new(&resolver, true);
        let group_write = converter
            .convert_resource_group_v1(&key, group_changes)
            .unwrap();

        // Deletion should still contain the metadata - for storage refunds.
        assert_eq!(group_write.metadata_op().metadata(), Some(&metadata));
        assert_eq!(group_write.metadata_op(), &WriteOp::DeletionWithMetadata {
            metadata
        });
        assert_none!(group_write.metadata_op().bytes());
    }
}
