use std::iter;

use cairo_lang_sierra::extensions::array::ArrayConcreteLibfunc;
use cairo_lang_sierra::extensions::boolean::BoolConcreteLibfunc;
use cairo_lang_sierra::extensions::boxing::BoxConcreteLibfunc;
use cairo_lang_sierra::extensions::builtin_cost::{
    BuiltinCostConcreteLibfunc, BuiltinCostWithdrawGasLibfunc, CostTokenType,
};
use cairo_lang_sierra::extensions::casts::CastConcreteLibfunc;
use cairo_lang_sierra::extensions::core::CoreConcreteLibfunc::{
    self, ApTracking, Array, Bitwise, Bool, Box, BranchAlign, BuiltinCost, Cast, Drop, Dup, Ec,
    Enum, Felt252, Felt252Dict, FunctionCall, Gas, Mem, Pedersen, Poseidon, Struct, Uint128,
    Uint16, Uint32, Uint64, Uint8, UnconditionalJump, UnwrapNonZero,
};
use cairo_lang_sierra::extensions::ec::EcConcreteLibfunc;
use cairo_lang_sierra::extensions::enm::EnumConcreteLibfunc;
use cairo_lang_sierra::extensions::felt252::{
    Felt252BinaryOperationConcrete, Felt252BinaryOperator, Felt252Concrete,
};
use cairo_lang_sierra::extensions::felt252_dict::Felt252DictConcreteLibfunc;
use cairo_lang_sierra::extensions::function_call::FunctionCallConcreteLibfunc;
use cairo_lang_sierra::extensions::gas::GasConcreteLibfunc::{
    GetAvailableGas, RedepositGas, WithdrawGas,
};
use cairo_lang_sierra::extensions::mem::MemConcreteLibfunc::{
    AllocLocal, FinalizeLocals, Rename, StoreLocal, StoreTemp,
};
use cairo_lang_sierra::extensions::nullable::NullableConcreteLibfunc;
use cairo_lang_sierra::extensions::pedersen::PedersenConcreteLibfunc;
use cairo_lang_sierra::extensions::poseidon::PoseidonConcreteLibfunc;
use cairo_lang_sierra::extensions::structure::StructConcreteLibfunc;
use cairo_lang_sierra::extensions::uint::{
    IntOperator, Uint16Concrete, Uint32Concrete, Uint64Concrete, Uint8Concrete,
};
use cairo_lang_sierra::extensions::uint128::Uint128Concrete;
use cairo_lang_sierra::extensions::ConcreteLibfunc;
use cairo_lang_sierra::ids::ConcreteTypeId;
use cairo_lang_sierra::program::Function;
use itertools::{chain, Itertools};

use crate::starknet_libfunc_cost_base::starknet_libfunc_cost_base;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConstCost {
    pub steps: i32,
    pub holes: i32,
    pub range_checks: i32,
}
impl ConstCost {
    pub const fn cost(&self) -> i32 {
        self.steps * 100 + self.holes * 10 + self.range_checks * 70
    }
}

/// Adds two [ConstCost] instances.
impl ConstCost {
    const fn add(self, rhs: Self) -> Self {
        Self {
            steps: self.steps + rhs.steps,
            holes: self.holes + rhs.holes,
            range_checks: self.range_checks + rhs.range_checks,
        }
    }
}

/// Adds two [ConstCost] instances.
impl std::ops::Add for ConstCost {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        self.add(rhs)
    }
}

// The costs of the dict_squash libfunc, divided into different parts.
/// The cost per each unique key in the dictionary.
pub const DICT_SQUASH_UNIQUE_KEY_COST: ConstCost =
    ConstCost { steps: 55, holes: 0, range_checks: 6 };
/// The cost per each access to a key after the first access.
pub const DICT_SQUASH_REPEATED_ACCESS_COST: ConstCost =
    ConstCost { steps: 9, holes: 0, range_checks: 1 };
/// The cost not dependent on the number of keys and access.
pub const DICT_SQUASH_FIXED_COST: ConstCost = ConstCost { steps: 75, holes: 0, range_checks: 3 };
/// The cost to charge per each read/write access. `DICT_SQUASH_UNIQUE_KEY_COST` is refunded for
/// each repeated access in dict_squash.
pub const DICT_SQUASH_ACCESS_COST: ConstCost =
    DICT_SQUASH_UNIQUE_KEY_COST.add(DICT_SQUASH_REPEATED_ACCESS_COST);

/// The operation required for extracting a libfunc's cost.
pub trait CostOperations {
    type CostType: Clone;

    /// Gets a cost from a constant value (of type [CostTokenType::Const]).
    fn const_cost(&self, value: ConstCost) -> Self::CostType {
        self.cost_token(value.cost(), CostTokenType::Const)
    }

    /// Gets a cost from step count.
    fn steps(&self, steps: i32) -> Self::CostType {
        self.const_cost(ConstCost { steps, ..ConstCost::default() })
    }

    /// Gets a cost from hole count.
    fn holes(&self, holes: i32) -> Self::CostType {
        self.const_cost(ConstCost { holes, ..ConstCost::default() })
    }

    /// Gets a cost from range check count.
    fn range_checks(&self, range_checks: i32) -> Self::CostType {
        self.const_cost(ConstCost { range_checks, ..ConstCost::default() })
    }

    /// Gets a cost of the given token type.
    fn cost_token(&self, count: i32, token_type: CostTokenType) -> Self::CostType;
    /// Gets a cost for the content of a function.
    fn function_token_cost(
        &mut self,
        function: &Function,
        token_type: CostTokenType,
    ) -> Self::CostType;
    /// Gets a cost for a variable for the current statement.
    fn statement_var_cost(&self, token_type: CostTokenType) -> Self::CostType;
    /// Adds costs.
    fn add(&self, lhs: Self::CostType, rhs: Self::CostType) -> Self::CostType;
    /// Subtracts costs.
    fn sub(&self, lhs: Self::CostType, rhs: Self::CostType) -> Self::CostType;
}

/// Trait for providing extra information required for calculating costs for a specific libfunc
/// invocation.
pub trait InvocationCostInfoProvider {
    /// Provides the sizes of types.
    fn type_size(&self, ty: &ConcreteTypeId) -> usize;
    /// Number of tokens provided by the libfunc invocation (currently only relevant for
    /// `withdraw_gas_all`).
    fn token_usages(&self, token_type: CostTokenType) -> usize;
    /// Provides the ap change variable value of the current statement.
    fn ap_change_var_value(&self) -> usize;
}

/// Returns a precost value for a libfunc - the cost of non-step tokens.
/// This is a helper function to implement costing both for creating
/// gas equations and getting actual gas cost after having a solution.
pub fn core_libfunc_precost<Ops: CostOperations>(
    ops: &mut Ops,
    libfunc: &CoreConcreteLibfunc,
) -> Vec<Ops::CostType> {
    match libfunc {
        FunctionCall(FunctionCallConcreteLibfunc { function, .. }) => {
            let func_content_cost = CostTokenType::iter_precost()
                .map(|token| ops.function_token_cost(function, *token))
                .collect_vec()
                .into_iter()
                .reduce(|x, y| ops.add(x, y));
            vec![func_content_cost.unwrap()]
        }
        Bitwise(_) => {
            vec![ops.cost_token(1, CostTokenType::Bitwise)]
        }
        Ec(EcConcreteLibfunc::StateAddMul(_)) => {
            vec![ops.cost_token(1, CostTokenType::EcOp)]
        }
        BranchAlign(_) => {
            vec![statement_vars_cost(ops, CostTokenType::iter_precost())]
        }
        Pedersen(libfunc) => match libfunc {
            PedersenConcreteLibfunc::PedersenHash(_) => {
                vec![ops.cost_token(1, CostTokenType::Pedersen)]
            }
        },
        Poseidon(libfunc) => match libfunc {
            PoseidonConcreteLibfunc::HadesPermutation(_) => {
                vec![ops.cost_token(1, CostTokenType::Poseidon)]
            }
        },
        BuiltinCost(BuiltinCostConcreteLibfunc::BuiltinWithdrawGas(_)) => {
            vec![
                ops.sub(ops.steps(0), statement_vars_cost(ops, CostTokenType::iter_precost())),
                ops.steps(0),
            ]
        }
        _ => libfunc.branch_signatures().iter().map(|_| ops.steps(0)).collect(),
    }
}

/// The cost of executing a libfunc for a specific output branch.
#[derive(Clone)]
pub enum BranchCost {
    /// A constant cost.
    Constant(ConstCost),
    /// A cost of a function call.
    FunctionCall { const_cost: ConstCost, function: Function },
    /// The cost of the `branch_align` libfunc.
    BranchAlign,
    /// The cost of `withdraw_gas` and `withdraw_gas_all` libfuncs.
    WithdrawGas { const_cost: ConstCost, success: bool, with_builtins: bool },
    /// The cost of the `redeposit_gas` libfunc.
    RedepositGas,
}

/// Converts [ConstCost] into [BranchCost].
impl From<ConstCost> for BranchCost {
    fn from(value: ConstCost) -> Self {
        BranchCost::Constant(value)
    }
}

/// Returns a postcost value for a libfunc - the cost of step token.
pub fn core_libfunc_postcost(
    libfunc: &CoreConcreteLibfunc,
    info_provider: &dyn InvocationCostInfoProvider,
) -> Vec<BranchCost> {
    let steps = |value| ConstCost { steps: value, ..Default::default() };
    let holes = |value| ConstCost { holes: value, ..Default::default() };
    let range_checks = |value| ConstCost { range_checks: value, ..Default::default() };
    match libfunc {
        FunctionCall(FunctionCallConcreteLibfunc { function, .. }) => {
            vec![BranchCost::FunctionCall { const_cost: steps(2), function: function.clone() }]
        }
        Bitwise(_) => {
            vec![steps(2).into()]
        }
        Bool(libfunc) => match libfunc {
            BoolConcreteLibfunc::And(_) => vec![steps(0).into()],
            BoolConcreteLibfunc::Not(_) => vec![steps(1).into()],
            BoolConcreteLibfunc::Xor(_) => vec![steps(1).into()],
            BoolConcreteLibfunc::Or(_) => vec![steps(2).into()],
            BoolConcreteLibfunc::ToFelt252(_) => vec![steps(0).into()],
        },
        Cast(libfunc) => match libfunc {
            CastConcreteLibfunc::Downcast(_) => {
                vec![(steps(3) + range_checks(1)).into(), (steps(4) + range_checks(1)).into()]
            }
            CastConcreteLibfunc::Upcast(_) => vec![steps(0).into()],
        },
        Ec(libfunc) => match libfunc {
            EcConcreteLibfunc::IsZero(_) => vec![steps(1).into(), steps(1).into()],
            EcConcreteLibfunc::Neg(_) => vec![steps(0).into()],
            EcConcreteLibfunc::StateAdd(_) => vec![steps(10).into()],
            EcConcreteLibfunc::TryNew(_) => vec![steps(7).into(), steps(7).into()],
            EcConcreteLibfunc::StateFinalize(_) => vec![steps(12).into(), steps(6).into()],
            EcConcreteLibfunc::StateInit(_) => vec![steps(8).into()],
            EcConcreteLibfunc::StateAddMul(_) => {
                vec![steps(5).into()]
            }
            EcConcreteLibfunc::PointFromX(_) => vec![
                (steps(14) + range_checks(3)).into(), // Success.
                steps(9).into(),                      // Failure.
            ],
            EcConcreteLibfunc::UnwrapPoint(_) => vec![steps(0).into()],
            EcConcreteLibfunc::Zero(_) => vec![steps(0).into()],
        },
        Gas(libfunc) => match libfunc {
            WithdrawGas(_) => vec![
                BranchCost::WithdrawGas {
                    const_cost: steps(3) + range_checks(1),
                    success: true,
                    with_builtins: false,
                },
                BranchCost::WithdrawGas {
                    const_cost: steps(4) + range_checks(1),
                    success: false,
                    with_builtins: false,
                },
            ],
            RedepositGas(_) => vec![BranchCost::RedepositGas],
            GetAvailableGas(_) => vec![steps(0).into()],
        },
        BranchAlign(_) => vec![BranchCost::BranchAlign],
        Array(libfunc) => match libfunc {
            ArrayConcreteLibfunc::New(_) => vec![steps(1).into()],
            ArrayConcreteLibfunc::Append(libfunc) => {
                vec![steps(info_provider.type_size(&libfunc.ty) as i32).into()]
            }
            ArrayConcreteLibfunc::PopFront(_)
            | ArrayConcreteLibfunc::SnapshotPopFront(_)
            | ArrayConcreteLibfunc::SnapshotPopBack(_) => vec![steps(2).into(), steps(3).into()],
            ArrayConcreteLibfunc::Get(libfunc) => {
                if info_provider.type_size(&libfunc.ty) == 1 {
                    vec![(steps(5) + range_checks(1)).into(), (steps(5) + range_checks(1)).into()]
                } else {
                    vec![(steps(6) + range_checks(1)).into(), (steps(6) + range_checks(1)).into()]
                }
            }
            ArrayConcreteLibfunc::Slice(libfunc) => {
                if info_provider.type_size(&libfunc.ty) == 1 {
                    vec![(steps(6) + range_checks(1)).into(), (steps(6) + range_checks(1)).into()]
                } else {
                    vec![(steps(8) + range_checks(1)).into(), (steps(7) + range_checks(1)).into()]
                }
            }
            ArrayConcreteLibfunc::Len(libfunc) => {
                vec![steps(if info_provider.type_size(&libfunc.ty) == 1 { 0 } else { 1 }).into()]
            }
        },
        Uint128(libfunc) => u128_libfunc_cost(libfunc).into_iter().map(BranchCost::from).collect(),
        Uint8(libfunc) => u8_libfunc_cost(libfunc).into_iter().map(BranchCost::from).collect(),
        Uint16(libfunc) => u16_libfunc_cost(libfunc).into_iter().map(BranchCost::from).collect(),
        Uint32(libfunc) => u32_libfunc_cost(libfunc).into_iter().map(BranchCost::from).collect(),
        Uint64(libfunc) => u64_libfunc_cost(libfunc).into_iter().map(BranchCost::from).collect(),
        Felt252(libfunc) => {
            felt252_libfunc_cost(libfunc).into_iter().map(BranchCost::from).collect()
        }
        Drop(_) | Dup(_) | ApTracking(_) | UnwrapNonZero(_) => {
            vec![steps(0).into()]
        }
        Box(libfunc) => match libfunc {
            BoxConcreteLibfunc::Into(libfunc) => {
                let n_steps =
                    std::cmp::max(1, info_provider.type_size(&libfunc.ty).try_into().unwrap());
                vec![steps(n_steps).into()]
            }
            BoxConcreteLibfunc::Unbox(_) => vec![steps(0).into()],
        },
        Mem(libfunc) => match libfunc {
            StoreTemp(libfunc) => vec![steps(info_provider.type_size(&libfunc.ty) as i32).into()],
            StoreLocal(libfunc) => {
                let size = info_provider.type_size(&libfunc.ty) as i32;
                vec![(steps(size) + holes(-size)).into()]
            }
            AllocLocal(libfunc) => {
                vec![holes(info_provider.type_size(&libfunc.ty) as i32).into()]
            }
            FinalizeLocals(_) => vec![steps(1).into()],
            Rename(_) => vec![steps(0).into()],
        },
        UnconditionalJump(_) => {
            vec![steps(1).into()]
        }
        Enum(libfunc) => match libfunc {
            EnumConcreteLibfunc::Init(_) => vec![steps(0).into()],
            EnumConcreteLibfunc::Match(sig) | EnumConcreteLibfunc::SnapshotMatch(sig) => {
                let n = sig.signature.branch_signatures.len();
                match n {
                    0 => vec![],
                    1 => vec![steps(0).into()],
                    2 => vec![steps(1).into(); 2],
                    _ => chain!(
                        iter::once(steps(1).into()),
                        itertools::repeat_n(steps(2).into(), n - 1)
                    )
                    .collect_vec(),
                }
            }
        },
        Struct(
            StructConcreteLibfunc::Construct(_)
            | StructConcreteLibfunc::Deconstruct(_)
            | StructConcreteLibfunc::SnapshotDeconstruct(_),
        ) => {
            vec![steps(0).into()]
        }
        Felt252Dict(libfunc) => match libfunc {
            Felt252DictConcreteLibfunc::New(_) => {
                vec![steps(9).into()]
            }
            Felt252DictConcreteLibfunc::Read(_) => {
                vec![(steps(3) + DICT_SQUASH_ACCESS_COST).into()]
            }
            Felt252DictConcreteLibfunc::Write(_) => {
                vec![(steps(2) + DICT_SQUASH_ACCESS_COST).into()]
            }
            Felt252DictConcreteLibfunc::Squash(_) => {
                // Dict squash have a fixed cost of 'DICT_SQUASH_CONST_COST' +
                // `DICT_SQUASH_ACCESS_COST` for each dict access. Only the fixed
                // cost is charged here, so that we would alway be able to call
                // squash even if running out of gas. The cost of the processing of
                // the first key is `DICT_SQUASH_ACCESS_COST`, and each access for
                // an existing key costs only 'DICT_SQUASH_REPEATED_ACCESS_COST'. In
                // each read/write we charge `DICT_SQUASH_ACCESS_COST` gas and
                // `DICT_SQUASH_ACCESS_COST - DICT_SQUASH_REPEATED_ACCESS_COST` gas are refunded per
                // each successive access in dict squash.
                vec![DICT_SQUASH_FIXED_COST.into()]
            }
        },
        Pedersen(libfunc) => match libfunc {
            PedersenConcreteLibfunc::PedersenHash(_) => vec![steps(2).into()],
        },
        Poseidon(libfunc) => match libfunc {
            PoseidonConcreteLibfunc::HadesPermutation(_) => vec![steps(3).into()],
        },
        BuiltinCost(builtin_libfunc) => match builtin_libfunc {
            BuiltinCostConcreteLibfunc::BuiltinWithdrawGas(_) => {
                vec![
                    BranchCost::WithdrawGas {
                        const_cost: steps(3) + range_checks(1),
                        success: true,
                        with_builtins: true,
                    },
                    BranchCost::WithdrawGas {
                        const_cost: steps(5) + range_checks(1),
                        success: false,
                        with_builtins: true,
                    },
                ]
            }
            BuiltinCostConcreteLibfunc::GetBuiltinCosts(_) => vec![steps(3).into()],
        },
        CoreConcreteLibfunc::StarkNet(libfunc) => {
            starknet_libfunc_cost_base(libfunc).into_iter().map(BranchCost::from).collect()
        }
        CoreConcreteLibfunc::Nullable(libfunc) => match libfunc {
            NullableConcreteLibfunc::Null(_) => vec![steps(0).into()],
            NullableConcreteLibfunc::NullableFromBox(_) => vec![steps(0).into()],
            NullableConcreteLibfunc::MatchNullable(_) => vec![steps(1).into(), steps(1).into()],
        },
        CoreConcreteLibfunc::Debug(_) => vec![steps(1).into()],
        CoreConcreteLibfunc::SnapshotTake(_) => vec![steps(0).into()],
    }
}

/// Returns a postcost value for a libfunc - the cost of step token.
/// This is a helper function to implement costing both for creating
/// gas equations and getting actual gas cost after having a solution.
// TODO(lior): Remove this function once it's not used.
pub fn core_libfunc_postcost_wrapper<
    Ops: CostOperations,
    InfoProvider: InvocationCostInfoProvider,
>(
    ops: &mut Ops,
    libfunc: &CoreConcreteLibfunc,
    info_provider: &InfoProvider,
) -> Vec<Ops::CostType> {
    let res = core_libfunc_postcost(libfunc, info_provider);
    res.into_iter()
        .map(|cost| match cost {
            BranchCost::Constant(const_cost) => ops.const_cost(const_cost),
            BranchCost::FunctionCall { const_cost, function } => {
                let func_content_cost = ops.function_token_cost(&function, CostTokenType::Const);
                ops.add(ops.const_cost(const_cost), func_content_cost)
            }
            BranchCost::BranchAlign => {
                let ap_change = info_provider.ap_change_var_value();
                let burnt_cost = ops.statement_var_cost(CostTokenType::Const);
                if ap_change == 0 {
                    burnt_cost
                } else {
                    ops.add(
                        burnt_cost,
                        ops.const_cost(ConstCost {
                            steps: 1,
                            holes: ap_change as i32,
                            range_checks: 0,
                        }),
                    )
                }
            }
            BranchCost::WithdrawGas { const_cost, success, with_builtins } => {
                let mut res = ops.const_cost(const_cost);
                if with_builtins {
                    let cost_computation =
                        BuiltinCostWithdrawGasLibfunc::cost_computation_steps(|token_type| {
                            info_provider.token_usages(token_type)
                        }) as i32;
                    res = ops.add(res, ops.steps(cost_computation));
                }
                if success {
                    res = ops.sub(res, ops.statement_var_cost(CostTokenType::Const));
                }
                res
            }
            BranchCost::RedepositGas => ops.statement_var_cost(CostTokenType::Const),
        })
        .collect()
}

/// Returns the sum of statement variables for all the requested tokens.
fn statement_vars_cost<'a, Ops: CostOperations, TokenTypes: Iterator<Item = &'a CostTokenType>>(
    ops: &Ops,
    token_types: TokenTypes,
) -> Ops::CostType {
    token_types
        .map(|token_type| ops.statement_var_cost(*token_type))
        .reduce(|x, y| ops.add(x, y))
        .unwrap()
}

/// Returns costs for u8 libfuncs.
fn u8_libfunc_cost(libfunc: &Uint8Concrete) -> Vec<ConstCost> {
    let steps = |value| ConstCost { steps: value, ..Default::default() };
    match libfunc {
        Uint8Concrete::Const(_) | Uint8Concrete::ToFelt252(_) | Uint8Concrete::WideMul(_) => {
            vec![steps(0)]
        }
        Uint8Concrete::Operation(libfunc) => match libfunc.operator {
            IntOperator::OverflowingAdd => {
                vec![
                    ConstCost { steps: 4, holes: 0, range_checks: 1 },
                    ConstCost { steps: 5, holes: 0, range_checks: 1 },
                ]
            }
            IntOperator::OverflowingSub => {
                vec![
                    ConstCost { steps: 3, holes: 0, range_checks: 1 },
                    ConstCost { steps: 6, holes: 0, range_checks: 1 },
                ]
            }
        },
        Uint8Concrete::LessThan(_) => {
            vec![
                ConstCost { steps: 3, holes: 0, range_checks: 1 },
                ConstCost { steps: 5, holes: 0, range_checks: 1 },
            ]
        }
        Uint8Concrete::SquareRoot(_) => {
            vec![ConstCost { steps: 9, holes: 0, range_checks: 4 }]
        }
        Uint8Concrete::Equal(_) => {
            vec![steps(2), steps(3)]
        }
        Uint8Concrete::LessThanOrEqual(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
            ]
        }
        Uint8Concrete::FromFelt252(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 2 },
                ConstCost { steps: 10, holes: 0, range_checks: 3 },
            ]
        }
        Uint8Concrete::IsZero(_) => vec![steps(1), steps(1)],
        Uint8Concrete::Divmod(_) => {
            vec![ConstCost { steps: 7, holes: 0, range_checks: 3 }]
        }
    }
}

/// Returns costs for u16 libfuncs.
fn u16_libfunc_cost(libfunc: &Uint16Concrete) -> Vec<ConstCost> {
    let steps = |value| ConstCost { steps: value, ..Default::default() };
    match libfunc {
        Uint16Concrete::Const(_) | Uint16Concrete::ToFelt252(_) | Uint16Concrete::WideMul(_) => {
            vec![steps(0)]
        }
        Uint16Concrete::Operation(libfunc) => match libfunc.operator {
            IntOperator::OverflowingAdd => {
                vec![
                    ConstCost { steps: 4, holes: 0, range_checks: 1 },
                    ConstCost { steps: 5, holes: 0, range_checks: 1 },
                ]
            }
            IntOperator::OverflowingSub => {
                vec![
                    ConstCost { steps: 3, holes: 0, range_checks: 1 },
                    ConstCost { steps: 6, holes: 0, range_checks: 1 },
                ]
            }
        },
        Uint16Concrete::LessThan(_) => {
            vec![
                ConstCost { steps: 3, holes: 0, range_checks: 1 },
                ConstCost { steps: 5, holes: 0, range_checks: 1 },
            ]
        }
        Uint16Concrete::SquareRoot(_) => {
            vec![ConstCost { steps: 9, holes: 0, range_checks: 4 }]
        }
        Uint16Concrete::Equal(_) => {
            vec![steps(2), steps(3)]
        }
        Uint16Concrete::LessThanOrEqual(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
            ]
        }
        Uint16Concrete::FromFelt252(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 2 },
                ConstCost { steps: 10, holes: 0, range_checks: 3 },
            ]
        }
        Uint16Concrete::IsZero(_) => vec![steps(1), steps(1)],
        Uint16Concrete::Divmod(_) => {
            vec![ConstCost { steps: 7, holes: 0, range_checks: 3 }]
        }
    }
}

/// Returns costs for u32 libfuncs.
fn u32_libfunc_cost(libfunc: &Uint32Concrete) -> Vec<ConstCost> {
    let steps = |value| ConstCost { steps: value, ..Default::default() };
    match libfunc {
        Uint32Concrete::Const(_) | Uint32Concrete::ToFelt252(_) | Uint32Concrete::WideMul(_) => {
            vec![steps(0)]
        }
        Uint32Concrete::Operation(libfunc) => match libfunc.operator {
            IntOperator::OverflowingAdd => {
                vec![
                    ConstCost { steps: 4, holes: 0, range_checks: 1 },
                    ConstCost { steps: 5, holes: 0, range_checks: 1 },
                ]
            }
            IntOperator::OverflowingSub => {
                vec![
                    ConstCost { steps: 3, holes: 0, range_checks: 1 },
                    ConstCost { steps: 6, holes: 0, range_checks: 1 },
                ]
            }
        },
        Uint32Concrete::LessThan(_) => {
            vec![
                ConstCost { steps: 3, holes: 0, range_checks: 1 },
                ConstCost { steps: 5, holes: 0, range_checks: 1 },
            ]
        }
        Uint32Concrete::SquareRoot(_) => {
            vec![ConstCost { steps: 9, holes: 0, range_checks: 4 }]
        }
        Uint32Concrete::Equal(_) => {
            vec![steps(2), steps(3)]
        }
        Uint32Concrete::LessThanOrEqual(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
            ]
        }
        Uint32Concrete::FromFelt252(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 2 },
                ConstCost { steps: 10, holes: 0, range_checks: 3 },
            ]
        }
        Uint32Concrete::IsZero(_) => vec![steps(1), steps(1)],
        Uint32Concrete::Divmod(_) => {
            vec![ConstCost { steps: 7, holes: 0, range_checks: 3 }]
        }
    }
}

/// Returns costs for u64 libfuncs.
fn u64_libfunc_cost(libfunc: &Uint64Concrete) -> Vec<ConstCost> {
    let steps = |value| ConstCost { steps: value, ..Default::default() };
    match libfunc {
        Uint64Concrete::Const(_) | Uint64Concrete::ToFelt252(_) | Uint64Concrete::WideMul(_) => {
            vec![steps(0)]
        }
        Uint64Concrete::Operation(libfunc) => match libfunc.operator {
            IntOperator::OverflowingAdd => {
                vec![
                    ConstCost { steps: 4, holes: 0, range_checks: 1 },
                    ConstCost { steps: 5, holes: 0, range_checks: 1 },
                ]
            }
            IntOperator::OverflowingSub => {
                vec![
                    ConstCost { steps: 3, holes: 0, range_checks: 1 },
                    ConstCost { steps: 6, holes: 0, range_checks: 1 },
                ]
            }
        },
        Uint64Concrete::LessThan(_) => {
            vec![
                ConstCost { steps: 3, holes: 0, range_checks: 1 },
                ConstCost { steps: 5, holes: 0, range_checks: 1 },
            ]
        }
        Uint64Concrete::SquareRoot(_) => {
            vec![ConstCost { steps: 9, holes: 0, range_checks: 4 }]
        }
        Uint64Concrete::Equal(_) => {
            vec![steps(2), steps(3)]
        }
        Uint64Concrete::LessThanOrEqual(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
            ]
        }
        Uint64Concrete::FromFelt252(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 2 },
                ConstCost { steps: 10, holes: 0, range_checks: 3 },
            ]
        }
        Uint64Concrete::IsZero(_) => vec![steps(1), steps(1)],
        Uint64Concrete::Divmod(_) => {
            vec![ConstCost { steps: 7, holes: 0, range_checks: 3 }]
        }
    }
}

/// Returns costs for u128 libfuncs.
fn u128_libfunc_cost(libfunc: &Uint128Concrete) -> Vec<ConstCost> {
    let steps = |value| ConstCost { steps: value, ..Default::default() };
    match libfunc {
        Uint128Concrete::Operation(libfunc) => match libfunc.operator {
            IntOperator::OverflowingAdd | IntOperator::OverflowingSub => {
                vec![
                    ConstCost { steps: 3, holes: 0, range_checks: 1 },
                    ConstCost { steps: 5, holes: 0, range_checks: 1 },
                ]
            }
        },
        Uint128Concrete::Divmod(_) => {
            vec![ConstCost { steps: 11, holes: 0, range_checks: 4 }]
        }
        Uint128Concrete::WideMul(_) => {
            vec![ConstCost { steps: 23, holes: 0, range_checks: 9 }]
        }
        Uint128Concrete::Const(_) | Uint128Concrete::ToFelt252(_) => {
            vec![Default::default()]
        }
        Uint128Concrete::FromFelt252(_) => {
            vec![
                ConstCost { steps: 2, holes: 0, range_checks: 1 },
                ConstCost { steps: 11, holes: 0, range_checks: 3 },
            ]
        }
        Uint128Concrete::IsZero(_) => {
            vec![steps(1), steps(1)]
        }
        Uint128Concrete::LessThan(_) => {
            vec![
                ConstCost { steps: 3, holes: 0, range_checks: 1 },
                ConstCost { steps: 5, holes: 0, range_checks: 1 },
            ]
        }
        Uint128Concrete::Equal(_) => {
            vec![steps(2), steps(3)]
        }
        Uint128Concrete::SquareRoot(_) => {
            vec![ConstCost { steps: 9, holes: 0, range_checks: 4 }]
        }
        Uint128Concrete::LessThanOrEqual(_) => {
            vec![
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
                ConstCost { steps: 4, holes: 0, range_checks: 1 },
            ]
        }
    }
}

/// Returns costs for felt252 libfuncs.
fn felt252_libfunc_cost(libfunc: &Felt252Concrete) -> Vec<ConstCost> {
    let steps = |value| ConstCost { steps: value, ..Default::default() };
    match libfunc {
        Felt252Concrete::BinaryOperation(bin_op) => {
            let op = match bin_op {
                Felt252BinaryOperationConcrete::WithVar(op) => op.operator,
                Felt252BinaryOperationConcrete::WithConst(op) => op.operator,
            };
            if op == Felt252BinaryOperator::Div { vec![steps(5)] } else { vec![steps(0)] }
        }
        Felt252Concrete::Const(_) => vec![steps(0)],
        Felt252Concrete::IsZero(_) => {
            vec![steps(1), steps(1)]
        }
    }
}
