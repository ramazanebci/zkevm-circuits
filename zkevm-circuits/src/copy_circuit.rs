//! The Copy circuit implements constraints and lookups for read-write steps for
//! copied bytes while execution opcodes such as CALLDATACOPY, CODECOPY, LOGS,
//! etc.
pub(crate) mod util;

#[cfg(any(test, feature = "test-circuits"))]
mod dev;
#[cfg(test)]
mod test;
#[cfg(feature = "test-circuits")]
pub use dev::CopyCircuit as TestCopyCircuit;

use crate::{
    evm_circuit::util::constraint_builder::{BaseConstraintBuilder, ConstrainBuilderCommon},
    table::{
        BytecodeFieldTag, BytecodeTable, CopyTable, LookupTable, RwTable, TxContextFieldTag,
        TxTable,
    },
    util::{Challenges, SubCircuit, SubCircuitConfig},
    witness,
    witness::{RwMap, Transaction},
};
use bus_mapping::{
    circuit_input_builder::{CopyDataType, CopyEvent},
    operation::Target,
    state_db::CodeDB,
};
use eth_types::Field;
use gadgets::{
    binary_number::{BinaryNumberChip, BinaryNumberConfig},
    less_than::{LtChip, LtConfig, LtInstruction},
    util::{and, not, or, Expr},
};
use halo2_proofs::{
    circuit::{Layouter, Region, Value},
    plonk::{
        Advice, Column, ConstraintSystem, Error, Expression, Fixed, SecondPhase, Selector,
        VirtualCells,
    },
    poly::Rotation,
};
use itertools::Itertools;
use std::marker::PhantomData;

// Rows to enable but not use, that can be queried safely by the last event.
const UNUSED_ROWS: usize = 2;
// Rows to disable, so they do not query into Halo2 reserved rows.
const DISABLED_ROWS: usize = 2;

/// The rw table shared between evm circuit and state circuit
#[derive(Clone, Debug)]
pub struct CopyCircuitConfig<F> {
    /// Whether this row denotes a step. A read row is a step and a write row is
    /// not.
    pub q_step: Selector,
    /// Whether the row is the last read-write pair for a copy event.
    pub is_last: Column<Advice>,
    /// The value copied in this copy step.
    pub value: Column<Advice>,
    /// Random linear combination accumulator value.
    pub value_acc_rlc: Column<Advice>,
    /// Whether the row is padding.
    pub is_pad: Column<Advice>,
    /// In case of a bytecode tag, this denotes whether or not the copied byte
    /// is an opcode or push data byte.
    pub is_code: Column<Advice>,
    /// Whether the row is enabled or not.
    pub q_enable: Column<Fixed>,
    /// The Copy Table contains the columns that are exposed via the lookup
    /// expressions
    pub copy_table: CopyTable,
    /// Lt chip to check: src_addr < src_addr_end.
    /// Since `src_addr` and `src_addr_end` are u64, 8 bytes are sufficient for
    /// the Lt chip.
    pub addr_lt_addr_end: LtConfig<F, 8>,
    // External tables
    /// TxTable
    pub tx_table: TxTable,
    /// RwTable
    pub rw_table: RwTable,
    /// BytecodeTable
    pub bytecode_table: BytecodeTable,
}

/// Circuit configuration arguments
pub struct CopyCircuitConfigArgs<F: Field> {
    /// TxTable
    pub tx_table: TxTable,
    /// RwTable
    pub rw_table: RwTable,
    /// BytecodeTable
    pub bytecode_table: BytecodeTable,
    /// CopyTable
    pub copy_table: CopyTable,
    /// q_enable
    pub q_enable: Column<Fixed>,
    /// Challenges
    pub challenges: Challenges<Expression<F>>,
}

impl<F: Field> SubCircuitConfig<F> for CopyCircuitConfig<F> {
    type ConfigArgs = CopyCircuitConfigArgs<F>;

    /// Configure the Copy Circuit constraining read-write steps and doing
    /// appropriate lookups to the Tx Table, RW Table and Bytecode Table.
    fn new(
        meta: &mut ConstraintSystem<F>,
        Self::ConfigArgs {
            tx_table,
            rw_table,
            bytecode_table,
            copy_table,
            q_enable,
            challenges,
        }: Self::ConfigArgs,
    ) -> Self {
        let q_step = meta.complex_selector();
        let is_last = meta.advice_column();
        let value = meta.advice_column();
        let value_acc_rlc = meta.advice_column_in(SecondPhase);
        let is_code = meta.advice_column();
        let is_pad = meta.advice_column();
        let is_first = copy_table.is_first;
        let id = copy_table.id;
        let addr = copy_table.addr;
        let src_addr_end = copy_table.src_addr_end;
        let bytes_left = copy_table.bytes_left;
        let rlc_acc = copy_table.rlc_acc;
        let rw_counter = copy_table.rw_counter;
        let rwc_inc_left = copy_table.rwc_inc_left;
        let tag = copy_table.tag;

        // annotate table columns
        tx_table.annotate_columns(meta);
        rw_table.annotate_columns(meta);
        bytecode_table.annotate_columns(meta);
        copy_table.annotate_columns(meta);

        let addr_lt_addr_end = LtChip::configure(
            meta,
            |meta| meta.query_selector(q_step),
            |meta| meta.query_advice(addr, Rotation::cur()),
            |meta| meta.query_advice(src_addr_end, Rotation::cur()),
        );

        meta.create_gate("verify row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_boolean(
                "is_first is boolean",
                meta.query_advice(is_first, Rotation::cur()),
            );
            cb.require_boolean(
                "is_last is boolean",
                meta.query_advice(is_last, Rotation::cur()),
            );
            cb.require_zero(
                "is_first == 0 when q_step == 0",
                and::expr([
                    not::expr(meta.query_selector(q_step)),
                    meta.query_advice(is_first, Rotation::cur()),
                ]),
            );
            cb.require_zero(
                "is_last == 0 when q_step == 1",
                and::expr([
                    meta.query_advice(is_last, Rotation::cur()),
                    meta.query_selector(q_step),
                ]),
            );

            constrain_must_terminate(&mut cb, meta, q_enable, &tag);

            let not_last_two_rows = 1.expr()
                - meta.query_advice(is_last, Rotation::cur())
                - meta.query_advice(is_last, Rotation::next());
            cb.condition(
                not_last_two_rows
                    * (not::expr(tag.value_equals(CopyDataType::Padding, Rotation::cur())(
                        meta,
                    ))),
                |cb| {
                    cb.require_equal_word(
                        "rows[0].id == rows[2].id",
                        id.map(|limb| meta.query_advice(limb, Rotation::cur())),
                        id.map(|limb| meta.query_advice(limb, Rotation(2))),
                    );
                    cb.require_equal(
                        "rows[0].tag == rows[2].tag",
                        tag.value(Rotation::cur())(meta),
                        tag.value(Rotation(2))(meta),
                    );
                    cb.require_equal(
                        "rows[0].addr + 1 == rows[2].addr",
                        meta.query_advice(addr, Rotation::cur()) + 1.expr(),
                        meta.query_advice(addr, Rotation(2)),
                    );
                    cb.require_equal(
                        "rows[0].src_addr_end == rows[2].src_addr_end for non-last step",
                        meta.query_advice(src_addr_end, Rotation::cur()),
                        meta.query_advice(src_addr_end, Rotation(2)),
                    );
                },
            );

            let rw_diff = and::expr([
                or::expr([
                    tag.value_equals(CopyDataType::Memory, Rotation::cur())(meta),
                    tag.value_equals(CopyDataType::TxLog, Rotation::cur())(meta),
                ]),
                not::expr(meta.query_advice(is_pad, Rotation::cur())),
            ]);
            cb.condition(
                not::expr(meta.query_advice(is_last, Rotation::cur())),
                |cb| {
                    cb.require_equal(
                        "rows[0].rw_counter + rw_diff == rows[1].rw_counter",
                        meta.query_advice(rw_counter, Rotation::cur()) + rw_diff.clone(),
                        meta.query_advice(rw_counter, Rotation::next()),
                    );
                    cb.require_equal(
                        "rows[0].rwc_inc_left - rw_diff == rows[1].rwc_inc_left",
                        meta.query_advice(rwc_inc_left, Rotation::cur()) - rw_diff.clone(),
                        meta.query_advice(rwc_inc_left, Rotation::next()),
                    );
                    cb.require_equal(
                        "rows[0].rlc_acc == rows[1].rlc_acc",
                        meta.query_advice(rlc_acc, Rotation::cur()),
                        meta.query_advice(rlc_acc, Rotation::next()),
                    );
                },
            );
            cb.condition(meta.query_advice(is_last, Rotation::cur()), |cb| {
                cb.require_equal(
                    "rwc_inc_left == rw_diff for last row in the copy slot",
                    meta.query_advice(rwc_inc_left, Rotation::cur()),
                    rw_diff,
                );
            });

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate(
            "Last Step (check value accumulator) Memory => Bytecode or RlcAcc",
            |meta: &mut halo2_proofs::plonk::VirtualCells<F>| {
                let mut cb = BaseConstraintBuilder::default();

                cb.require_equal(
                    "value_acc_rlc == rlc_acc on the last row",
                    meta.query_advice(value_acc_rlc, Rotation::next()),
                    meta.query_advice(rlc_acc, Rotation::next()),
                );

                cb.gate(and::expr([
                    meta.query_fixed(q_enable, Rotation::cur()),
                    meta.query_advice(is_last, Rotation::next()),
                    // To build a selector expression just having 0 when false and != 0 when true
                    // is enough, so we could replace the `or` by a `+`. This
                    // would give 2 when both expressions are true
                    // but it's fine.
                    and::expr([
                        tag.value_equals(CopyDataType::Memory, Rotation::cur())(meta),
                        tag.value_equals(CopyDataType::Bytecode, Rotation::next())(meta),
                    ]) + tag.value_equals(CopyDataType::RlcAcc, Rotation::next())(meta),
                ]))
            },
        );

        meta.create_gate("verify step (q_step == 1)", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_zero(
                "bytes_left == 1 for last step",
                and::expr([
                    meta.query_advice(is_last, Rotation::next()),
                    1.expr() - meta.query_advice(bytes_left, Rotation::cur()),
                ]),
            );
            cb.condition(
                not::expr(or::expr([
                    meta.query_advice(is_last, Rotation::next()),
                    tag.value_equals(CopyDataType::Padding, Rotation::cur())(meta),
                ])),
                |cb| {
                    cb.require_equal(
                        "bytes_left == bytes_left_next + 1 for non-last step",
                        meta.query_advice(bytes_left, Rotation::cur()),
                        meta.query_advice(bytes_left, Rotation(2)) + 1.expr(),
                    );
                },
            );
            cb.condition(meta.query_advice(is_first, Rotation::cur()), |cb| {
                cb.require_equal(
                    "value == value_acc_rlc at every first copy event",
                    meta.query_advice(value, Rotation::cur()),
                    meta.query_advice(value_acc_rlc, Rotation::cur()),
                );
            });
            cb.require_equal(
                "write value == read value",
                meta.query_advice(value, Rotation::cur()),
                meta.query_advice(value, Rotation::next()),
            );
            cb.require_equal(
                "value_acc_rlc is same for read-write rows",
                meta.query_advice(value_acc_rlc, Rotation::cur()),
                meta.query_advice(value_acc_rlc, Rotation::next()),
            );
            cb.condition(
                and::expr([
                    not::expr(meta.query_advice(is_last, Rotation::next())),
                    not::expr(meta.query_advice(is_pad, Rotation::cur())),
                ]),
                |cb| {
                    cb.require_equal(
                        "value_acc_rlc(2) == value_acc_rlc(0) * r + value(2)",
                        meta.query_advice(value_acc_rlc, Rotation(2)),
                        meta.query_advice(value_acc_rlc, Rotation::cur())
                            * challenges.keccak_input()
                            + meta.query_advice(value, Rotation(2)),
                    );
                },
            );
            cb.require_zero(
                "value == 0 when is_pad == 1 for read",
                and::expr([
                    meta.query_advice(is_pad, Rotation::cur()),
                    meta.query_advice(value, Rotation::cur()),
                ]),
            );
            cb.require_equal(
                "is_pad == 1 - (src_addr < src_addr_end) for read row",
                1.expr() - addr_lt_addr_end.is_lt(meta, None),
                meta.query_advice(is_pad, Rotation::cur()),
            );
            cb.require_zero(
                "is_pad == 0 for write row",
                meta.query_advice(is_pad, Rotation::next()),
            );

            cb.gate(and::expr([meta.query_selector(q_step)]))
        });

        meta.lookup_any("Memory lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::Memory, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            vec![
                meta.query_advice(rw_counter, Rotation::cur()),
                not::expr(meta.query_selector(q_step)),
                Target::Memory.expr(),
                meta.query_advice(id.lo(), Rotation::cur()), // call_id
                meta.query_advice(addr, Rotation::cur()),    // memory address
                0.expr(),                                    // field tag
                0.expr(),                                    // storage_key_lo
                0.expr(),                                    // storage_key_hi
                meta.query_advice(value, Rotation::cur()),   // value_lo
                0.expr(),                                    // value_hi
                0.expr(),                                    // value_prev_lo
                0.expr(),                                    // value_prev_hi
                0.expr(),                                    // init_val_lo
                0.expr(),                                    // init_val_hi
            ]
            .into_iter()
            .zip_eq(rw_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        meta.lookup_any("TxLog lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::TxLog, Rotation::cur())(meta);
            vec![
                meta.query_advice(rw_counter, Rotation::cur()),
                1.expr(),
                Target::TxLog.expr(),
                meta.query_advice(id.lo(), Rotation::cur()), // tx_id
                meta.query_advice(addr, Rotation::cur()),    // byte_index || field_tag || log_id
                0.expr(),                                    // field tag
                0.expr(),                                    // storage_key_lo
                0.expr(),                                    // storage_key_hi
                meta.query_advice(value, Rotation::cur()),   // value_lo
                0.expr(),                                    // value_hi
                0.expr(),                                    // value_prev_lo
                0.expr(),                                    // value_prev_hi
                0.expr(),                                    // init_val_lo
                0.expr(),                                    // init_val_hi
            ]
            .into_iter()
            .zip_eq(rw_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        meta.lookup_any("Bytecode lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::Bytecode, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            vec![
                meta.query_advice(id.lo(), Rotation::cur()),
                meta.query_advice(id.hi(), Rotation::cur()),
                BytecodeFieldTag::Byte.expr(),
                meta.query_advice(addr, Rotation::cur()),
                meta.query_advice(is_code, Rotation::cur()),
                meta.query_advice(value, Rotation::cur()),
            ]
            .into_iter()
            .zip_eq(bytecode_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        meta.lookup_any("Tx calldata lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::TxCalldata, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            vec![
                meta.query_advice(id.lo(), Rotation::cur()), /* For transaction ID we use lo
                                                              * limb only */
                TxContextFieldTag::CallData.expr(),
                meta.query_advice(addr, Rotation::cur()),
                meta.query_advice(value, Rotation::cur()),
            ]
            .into_iter()
            .zip(tx_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        meta.create_gate("id_hi === 0 when Momory", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let cond = tag.value_equals(CopyDataType::Memory, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            cb.condition(cond, |cb| {
                cb.require_zero("id_hi === 0", meta.query_advice(id.hi(), Rotation::cur()))
            });
            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("id_hi === 0 when TxLog", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let cond = tag.value_equals(CopyDataType::TxLog, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            cb.condition(cond, |cb| {
                cb.require_zero("id_hi === 0", meta.query_advice(id.hi(), Rotation::cur()))
            });
            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("id_hi === 0 when TxCalldata", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let cond = tag.value_equals(CopyDataType::TxCalldata, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            cb.condition(cond, |cb| {
                cb.require_zero("id_hi === 0", meta.query_advice(id.hi(), Rotation::cur()))
            });
            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        Self {
            q_step,
            is_last,
            value,
            value_acc_rlc,
            is_pad,
            is_code,
            q_enable,
            addr_lt_addr_end,
            copy_table,
            tx_table,
            rw_table,
            bytecode_table,
        }
    }
}

/// Verify that is_last goes to 1 at some point.
pub fn constrain_must_terminate<F: Field>(
    cb: &mut BaseConstraintBuilder<F>,
    meta: &mut VirtualCells<'_, F>,
    q_enable: Column<Fixed>,
    tag: &BinaryNumberConfig<CopyDataType, 3>,
) {
    // If an event has started (tag != Padding on reader and writer rows), require q_enable=1 at the
    // next step. This prevents querying rows where constraints are disabled.
    //
    // The tag is then copied to the next step by "rows[0].tag == rows[2].tag". Eventually,
    // q_enable=0. By that point the tag must have switched to Padding, which is only possible with
    // is_last=1. This guarantees that all the final conditions are checked.
    let is_event = tag.value(Rotation::cur())(meta) - tag.constant_expr::<F>(CopyDataType::Padding);
    cb.condition(is_event, |cb| {
        cb.require_equal(
            "the next step is enabled",
            meta.query_fixed(q_enable, Rotation(2)),
            1.expr(),
        );
    });
}

impl<F: Field> CopyCircuitConfig<F> {
    /// Assign an individual copy event to the Copy Circuit.
    pub fn assign_copy_event(
        &self,
        region: &mut Region<F>,
        offset: &mut usize,
        tag_chip: &BinaryNumberChip<F, CopyDataType, 3>,
        lt_chip: &LtChip<F, 8>,
        challenges: Challenges<Value<F>>,
        copy_event: &CopyEvent,
    ) -> Result<(), Error> {
        for (step_idx, (tag, table_row, circuit_row)) in
            CopyTable::assignments(copy_event, challenges)
                .iter()
                .enumerate()
        {
            let is_read = step_idx % 2 == 0;

            // Copy table assignments
            for (&column, &(value, label)) in
                <CopyTable as LookupTable<F>>::advice_columns(&self.copy_table)
                    .iter()
                    .zip_eq(table_row)
            {
                // Leave sr_addr_end and bytes_left unassigned when !is_read
                if !is_read && (label == "src_addr_end" || label == "bytes_left") {
                } else {
                    region.assign_advice(
                        || format!("{} at row: {}", label, offset),
                        column,
                        *offset,
                        || value,
                    )?;
                }
            }

            // q_step
            if is_read {
                self.q_step.enable(region, *offset)?;
            }
            // q_enable
            region.assign_fixed(
                || "q_enable",
                self.q_enable,
                *offset,
                || Value::known(F::ONE),
            )?;

            // is_last, value, is_pad, is_code
            for (column, &(value, label)) in [
                self.is_last,
                self.value,
                self.value_acc_rlc,
                self.is_pad,
                self.is_code,
            ]
            .iter()
            .zip_eq(circuit_row)
            {
                region.assign_advice(
                    || format!("{} at row: {}", label, *offset),
                    *column,
                    *offset,
                    || value,
                )?;
            }

            // tag
            tag_chip.assign(region, *offset, tag)?;

            // lt chip
            if is_read {
                lt_chip.assign(
                    region,
                    *offset,
                    Value::known(F::from(
                        copy_event.src_addr + u64::try_from(step_idx).unwrap() / 2u64,
                    )),
                    Value::known(F::from(copy_event.src_addr_end)),
                )?;
            }

            *offset += 1;
        }

        Ok(())
    }

    /// Assign vec of copy events
    pub fn assign_copy_events(
        &self,
        layouter: &mut impl Layouter<F>,
        copy_events: &[CopyEvent],
        max_copy_rows: usize,
        challenges: Challenges<Value<F>>,
    ) -> Result<(), Error> {
        let copy_rows_needed = copy_events.iter().map(|c| c.bytes.len() * 2).sum::<usize>();

        assert!(
            copy_rows_needed + DISABLED_ROWS + UNUSED_ROWS <= max_copy_rows,
            "copy rows not enough {copy_rows_needed} + 4 vs {max_copy_rows}"
        );
        let filler_rows = max_copy_rows - copy_rows_needed - DISABLED_ROWS;

        let tag_chip = BinaryNumberChip::construct(self.copy_table.tag);
        let lt_chip = LtChip::construct(self.addr_lt_addr_end);

        lt_chip.load(layouter)?;

        layouter.assign_region(
            || "assign copy table",
            |mut region| {
                region.name_column(|| "is_last", self.is_last);
                region.name_column(|| "value", self.value);
                region.name_column(|| "is_code", self.is_code);
                region.name_column(|| "is_pad", self.is_pad);

                let mut offset = 0;
                for copy_event in copy_events.iter() {
                    self.assign_copy_event(
                        &mut region,
                        &mut offset,
                        &tag_chip,
                        &lt_chip,
                        challenges,
                        copy_event,
                    )?;
                }

                for _ in 0..filler_rows {
                    self.assign_padding_row(&mut region, &mut offset, true, &tag_chip, &lt_chip)?;
                }
                assert_eq!(offset % 2, 0, "enabled rows must come in pairs");

                for _ in 0..DISABLED_ROWS {
                    self.assign_padding_row(&mut region, &mut offset, false, &tag_chip, &lt_chip)?;
                }

                Ok(())
            },
        )
    }

    fn assign_padding_row(
        &self,
        region: &mut Region<F>,
        offset: &mut usize,
        enabled: bool,
        tag_chip: &BinaryNumberChip<F, CopyDataType, 3>,
        lt_chip: &LtChip<F, 8>,
    ) -> Result<(), Error> {
        // q_enable
        region.assign_fixed(
            || "q_enable",
            self.q_enable,
            *offset,
            || Value::known(if enabled { F::ONE } else { F::ZERO }),
        )?;
        // q_step
        if enabled && *offset % 2 == 0 {
            self.q_step.enable(region, *offset)?;
        }

        // is_first
        region.assign_advice(
            || format!("assign is_first {}", *offset),
            self.copy_table.is_first,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // is_last
        region.assign_advice(
            || format!("assign is_last {}", *offset),
            self.is_last,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // id
        region.assign_advice(
            || format!("assign id lo {}", *offset),
            self.copy_table.id.lo(),
            *offset,
            || Value::known(F::ZERO),
        )?;
        region.assign_advice(
            || format!("assign id hi {}", *offset),
            self.copy_table.id.hi(),
            *offset,
            || Value::known(F::ZERO),
        )?;
        // addr
        region.assign_advice(
            || format!("assign addr {}", *offset),
            self.copy_table.addr,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // src_addr_end
        region.assign_advice(
            || format!("assign src_addr_end {}", *offset),
            self.copy_table.src_addr_end,
            *offset,
            || Value::known(F::ONE),
        )?;
        // bytes_left
        region.assign_advice(
            || format!("assign bytes_left {}", *offset),
            self.copy_table.bytes_left,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // value
        region.assign_advice(
            || format!("assign value {}", *offset),
            self.value,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // value_acc_rlc
        region.assign_advice(
            || format!("assign value_acc_rlc {}", *offset),
            self.value_acc_rlc,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // rlc_acc
        region.assign_advice(
            || format!("assign rlc_acc {}", *offset),
            self.copy_table.rlc_acc,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // is_code
        region.assign_advice(
            || format!("assign is_code {}", *offset),
            self.is_code,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // is_pad
        region.assign_advice(
            || format!("assign is_pad {}", *offset),
            self.is_pad,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // rw_counter
        region.assign_advice(
            || format!("assign rw_counter {}", *offset),
            self.copy_table.rw_counter,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // rwc_inc_left
        region.assign_advice(
            || format!("assign rwc_inc_left {}", *offset),
            self.copy_table.rwc_inc_left,
            *offset,
            || Value::known(F::ZERO),
        )?;
        // tag
        tag_chip.assign(region, *offset, &CopyDataType::Padding)?;
        // Assign LT gadget
        lt_chip.assign(region, *offset, Value::known(F::ZERO), Value::known(F::ONE))?;

        *offset += 1;

        Ok(())
    }
}

/// Struct for external data, specifies values for related lookup tables
#[derive(Clone, Debug, Default)]
pub struct ExternalData {
    /// TxCircuit -> max_txs
    pub max_txs: usize,
    /// TxCircuit -> max_calldata
    pub max_calldata: usize,
    /// TxCircuit -> txs
    pub txs: Vec<Transaction>,
    /// StateCircuit -> max_rws
    pub max_rws: usize,
    /// StateCircuit -> rws
    pub rws: RwMap,
    /// BytecodeCircuit -> bytecodes
    pub bytecodes: CodeDB,
}

/// Copy Circuit
#[derive(Clone, Debug, Default)]
pub struct CopyCircuit<F: Field> {
    /// Copy events
    pub copy_events: Vec<CopyEvent>,
    /// Max number of rows in copy circuit
    pub max_copy_rows: usize,
    _marker: PhantomData<F>,
    /// Data for external lookup tables
    pub external_data: ExternalData,
}

impl<F: Field> CopyCircuit<F> {
    /// Return a new CopyCircuit
    pub fn new(copy_events: Vec<CopyEvent>, max_copy_rows: usize) -> Self {
        Self {
            copy_events,
            max_copy_rows,
            _marker: PhantomData::default(),
            external_data: ExternalData::default(),
        }
    }

    /// Return a new CopyCircuit with external data
    pub fn new_with_external_data(
        copy_events: Vec<CopyEvent>,
        max_copy_rows: usize,
        external_data: ExternalData,
    ) -> Self {
        Self {
            copy_events,
            max_copy_rows,
            _marker: PhantomData::default(),
            external_data,
        }
    }

    /// Return a new CopyCircuit from a block without the external data required
    /// to assign lookup tables.  This constructor is only suitable to be
    /// used by the SuperCircuit, which already assigns the external lookup
    /// tables.
    pub fn new_from_block_no_external(block: &witness::Block<F>) -> Self {
        Self::new(
            block.copy_events.clone(),
            block.circuits_params.max_copy_rows,
        )
    }
}

impl<F: Field> SubCircuit<F> for CopyCircuit<F> {
    type Config = CopyCircuitConfig<F>;

    fn unusable_rows() -> usize {
        // No column queried at more than 3 distinct rotations, so returns 6 as
        // minimum unusable rows.
        6
    }

    fn new_from_block(block: &witness::Block<F>) -> Self {
        Self::new_with_external_data(
            block.copy_events.clone(),
            block.circuits_params.max_copy_rows,
            ExternalData {
                max_txs: block.circuits_params.max_txs,
                max_calldata: block.circuits_params.max_calldata,
                txs: block.txs.clone(),
                max_rws: block.circuits_params.max_rws,
                rws: block.rws.clone(),
                bytecodes: block.bytecodes.clone(),
            },
        )
    }

    /// Return the minimum number of rows required to prove the block
    fn min_num_rows_block(block: &witness::Block<F>) -> (usize, usize) {
        (
            block
                .copy_events
                .iter()
                .map(|c| c.bytes.len() * 2)
                .sum::<usize>()
                + 2,
            block.circuits_params.max_copy_rows,
        )
    }

    /// Make the assignments to the CopyCircuit
    fn synthesize_sub(
        &self,
        config: &Self::Config,
        challenges: &Challenges<Value<F>>,
        layouter: &mut impl Layouter<F>,
    ) -> Result<(), Error> {
        config.assign_copy_events(layouter, &self.copy_events, self.max_copy_rows, *challenges)
    }
}
