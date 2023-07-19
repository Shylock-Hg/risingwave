// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module contains datatypes and functions which can be generated by sqlsmith.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use itertools::Itertools;
use risingwave_common::types::{DataType, DataTypeName};
use risingwave_expr::agg::AggKind;
use risingwave_expr::sig::agg::{agg_func_sigs, AggFuncSig as RwAggFuncSig};
use risingwave_expr::sig::cast::{cast_sigs, CastContext, CastSig as RwCastSig};
use risingwave_expr::sig::func::{func_sigs, FuncSign as RwFuncSig};
use risingwave_frontend::expr::ExprType;
use risingwave_sqlparser::ast::{BinaryOperator, DataType as AstDataType, StructField};

pub(super) fn data_type_to_ast_data_type(data_type: &DataType) -> AstDataType {
    match data_type {
        DataType::Boolean => AstDataType::Boolean,
        DataType::Int16 => AstDataType::SmallInt,
        DataType::Int32 => AstDataType::Int,
        DataType::Int64 => AstDataType::BigInt,
        DataType::Int256 => AstDataType::Custom(vec!["rw_int256".into()].into()),
        DataType::Serial => unreachable!("serial should not be generated"),
        DataType::Decimal => AstDataType::Decimal(None, None),
        DataType::Float32 => AstDataType::Real,
        DataType::Float64 => AstDataType::Double,
        DataType::Varchar => AstDataType::Varchar,
        DataType::Bytea => AstDataType::Bytea,
        DataType::Date => AstDataType::Date,
        DataType::Timestamp => AstDataType::Timestamp(false),
        DataType::Timestamptz => AstDataType::Timestamp(true),
        DataType::Time => AstDataType::Time(false),
        DataType::Interval => AstDataType::Interval,
        DataType::Jsonb => AstDataType::Custom(vec!["JSONB".into()].into()),
        DataType::Struct(inner) => AstDataType::Struct(
            inner
                .iter()
                .map(|(name, typ)| StructField {
                    name: name.into(),
                    data_type: data_type_to_ast_data_type(typ),
                })
                .collect(),
        ),
        DataType::List(ref typ) => AstDataType::Array(Box::new(data_type_to_ast_data_type(typ))),
    }
}

fn data_type_name_to_ast_data_type(data_type_name: &DataTypeName) -> Option<DataType> {
    use DataTypeName as T;
    match data_type_name {
        T::Boolean => Some(DataType::Boolean),
        T::Int16 => Some(DataType::Int16),
        T::Int32 => Some(DataType::Int32),
        T::Int64 => Some(DataType::Int64),
        T::Decimal => Some(DataType::Decimal),
        T::Float32 => Some(DataType::Float32),
        T::Float64 => Some(DataType::Float64),
        T::Varchar => Some(DataType::Varchar),
        T::Date => Some(DataType::Date),
        T::Timestamp => Some(DataType::Timestamp),
        T::Timestamptz => Some(DataType::Timestamptz),
        T::Time => Some(DataType::Time),
        T::Interval => Some(DataType::Interval),
        _ => None,
    }
}

/// Provide internal `CastSig` which can be used for `struct` and `list`.
#[derive(Clone)]
pub struct CastSig {
    pub from_type: DataType,
    pub to_type: DataType,
    pub context: CastContext,
}

impl TryFrom<RwCastSig> for CastSig {
    type Error = String;

    fn try_from(value: RwCastSig) -> Result<Self, Self::Error> {
        if let Some(from_type) = data_type_name_to_ast_data_type(&value.from_type)
            && let Some(to_type) = data_type_name_to_ast_data_type(&value.to_type)
        {
            Ok(CastSig {
                from_type,
                to_type,
                context: value.context,
            })
        } else {
            Err(format!("unsupported cast sig: {:?}", value))
        }
    }
}

/// Provide internal `FuncSig` which can be used for `struct` and `list`.
#[derive(Clone)]
pub struct FuncSig {
    pub func: ExprType,
    pub inputs_type: Vec<DataType>,
    pub ret_type: DataType,
}

impl TryFrom<&RwFuncSig> for FuncSig {
    type Error = String;

    fn try_from(value: &RwFuncSig) -> Result<Self, Self::Error> {
        if let Some(inputs_type) = value
            .inputs_type
            .iter()
            .map(data_type_name_to_ast_data_type)
            .collect()
            && let Some(ret_type) = data_type_name_to_ast_data_type(&value.ret_type)
        {
            Ok(FuncSig {
                inputs_type,
                ret_type,
                func: value.func,
            })
        } else {
            Err(format!("unsupported func sig: {:?}", value))
        }
    }
}

/// Provide internal `AggFuncSig` which can be used for `struct` and `list`.
#[derive(Clone)]
pub struct AggFuncSig {
    pub func: AggKind,
    pub inputs_type: Vec<DataType>,
    pub ret_type: DataType,
}

impl TryFrom<&RwAggFuncSig> for AggFuncSig {
    type Error = String;

    fn try_from(value: &RwAggFuncSig) -> Result<Self, Self::Error> {
        if let Some(inputs_type) = value
            .inputs_type
            .iter()
            .map(data_type_name_to_ast_data_type)
            .collect()
            && let Some(ret_type) = data_type_name_to_ast_data_type(&value.ret_type)
        {
            Ok(AggFuncSig {
                inputs_type,
                ret_type,
                func: value.func,
            })
        } else {
            Err(format!("unsupported agg_func sig: {:?}", value))
        }
    }
}

/// Function ban list.
/// These functions should be generated eventually, by adding expression constraints.
/// If we naively generate arguments for these functions, it will affect sqlsmith
/// effectiveness, e.g. cause it to crash.
static FUNC_BAN_LIST: LazyLock<HashSet<ExprType>> = LazyLock::new(|| {
    [
        ExprType::Repeat, // FIXME: https://github.com/risingwavelabs/risingwave/issues/8003
    ]
    .into_iter()
    .collect()
});

/// Table which maps functions' return types to possible function signatures.
// ENABLE: https://github.com/risingwavelabs/risingwave/issues/5826
pub(crate) static FUNC_TABLE: LazyLock<HashMap<DataType, Vec<FuncSig>>> = LazyLock::new(|| {
    let mut funcs = HashMap::<DataType, Vec<FuncSig>>::new();
    func_sigs()
        .filter(|func| {
            func.inputs_type
                .iter()
                .all(|t| *t != DataTypeName::Timestamptz)
                && !FUNC_BAN_LIST.contains(&func.func)
        })
        .filter_map(|func| func.try_into().ok())
        .for_each(|func: FuncSig| funcs.entry(func.ret_type.clone()).or_default().push(func));
    funcs
});

/// Set of invariant functions
// ENABLE: https://github.com/risingwavelabs/risingwave/issues/5826
pub(crate) static INVARIANT_FUNC_SET: LazyLock<HashSet<ExprType>> = LazyLock::new(|| {
    func_sigs()
        .map(|sig| sig.func)
        .counts()
        .into_iter()
        .filter(|(_key, count)| *count == 1)
        .map(|(key, _)| key)
        .collect()
});

/// Table which maps aggregate functions' return types to possible function signatures.
// ENABLE: https://github.com/risingwavelabs/risingwave/issues/5826
pub(crate) static AGG_FUNC_TABLE: LazyLock<HashMap<DataType, Vec<AggFuncSig>>> = LazyLock::new(
    || {
        let mut funcs = HashMap::<DataType, Vec<AggFuncSig>>::new();
        agg_func_sigs()
            .filter(|func| {
                func.inputs_type
                    .iter()
                    .all(|t| *t != DataTypeName::Timestamptz)
                    // Ignored functions
                    && ![
                        AggKind::Sum0, // Used internally
                        AggKind::BitAnd,
                        AggKind::BitOr,
                        AggKind::BoolAnd,
                        AggKind::BoolOr,
                        AggKind::PercentileCont,
                        AggKind::PercentileDisc,
                        AggKind::Mode,
                    ]
                    .contains(&func.func)
                    // Exclude 2 phase agg global sum.
                    // Sum(Int64) -> Int64.
                    // Otherwise it conflicts with normal aggregation:
                    // Sum(Int64) -> Decimal.
                    // And sqlsmith will generate expressions with wrong types.
                    && if func.func == AggKind::Sum {
                       !(func.inputs_type[0] == DataTypeName::Int64 && func.ret_type == DataTypeName::Int64)
                    } else {
                       true
                    }
            })
            .filter_map(|func| func.try_into().ok())
            .for_each(|func: AggFuncSig| {
                funcs.entry(func.ret_type.clone()).or_default().push(func)
            });
        funcs
    },
);

/// Build a cast map from return types to viable cast-signatures.
/// NOTE: We avoid cast from varchar to other datatypes apart from itself.
/// This is because arbitrary strings may not be able to cast,
/// creating large number of invalid queries.
pub(crate) static EXPLICIT_CAST_TABLE: LazyLock<HashMap<DataType, Vec<CastSig>>> =
    LazyLock::new(|| {
        let mut casts = HashMap::<DataType, Vec<CastSig>>::new();
        cast_sigs()
            .filter_map(|cast| cast.try_into().ok())
            .filter(|cast: &CastSig| cast.context == CastContext::Explicit)
            .filter(|cast| cast.from_type != DataType::Varchar || cast.to_type == DataType::Varchar)
            .for_each(|cast| casts.entry(cast.to_type.clone()).or_default().push(cast));
        casts
    });

/// Build a cast map from return types to viable cast-signatures.
/// NOTE: We avoid cast from varchar to other datatypes apart from itself.
/// This is because arbitrary strings may not be able to cast,
/// creating large number of invalid queries.
pub(crate) static IMPLICIT_CAST_TABLE: LazyLock<HashMap<DataType, Vec<CastSig>>> =
    LazyLock::new(|| {
        let mut casts = HashMap::<DataType, Vec<CastSig>>::new();
        cast_sigs()
            .filter_map(|cast| cast.try_into().ok())
            .filter(|cast: &CastSig| cast.context == CastContext::Implicit)
            .filter(|cast| cast.from_type != DataType::Varchar || cast.to_type == DataType::Varchar)
            .for_each(|cast| casts.entry(cast.to_type.clone()).or_default().push(cast));
        casts
    });

fn expr_type_to_inequality_op(typ: ExprType) -> Option<BinaryOperator> {
    match typ {
        ExprType::GreaterThan => Some(BinaryOperator::Gt),
        ExprType::GreaterThanOrEqual => Some(BinaryOperator::GtEq),
        ExprType::LessThan => Some(BinaryOperator::Lt),
        ExprType::LessThanOrEqual => Some(BinaryOperator::LtEq),
        ExprType::NotEqual => Some(BinaryOperator::NotEq),
        _ => None,
    }
}

/// Build set of binary inequality functions like `>`, `<`, etc...
/// Maps from LHS, RHS argument to Inequality Operation
/// For instance:
/// GreaterThanOrEqual(Int16, Int64) -> Boolean
/// Will store an entry of:
/// Key: Int16, Int64
/// Value: `BinaryOp::GreaterThanOrEqual`
/// in the table.
pub(crate) static BINARY_INEQUALITY_OP_TABLE: LazyLock<
    HashMap<(DataType, DataType), Vec<BinaryOperator>>,
> = LazyLock::new(|| {
    let mut funcs = HashMap::<(DataType, DataType), Vec<BinaryOperator>>::new();
    func_sigs()
        .filter(|func| {
            !FUNC_BAN_LIST.contains(&func.func)
                && func.ret_type == DataTypeName::Boolean
                && func.inputs_type.len() == 2
                && func
                    .inputs_type
                    .iter()
                    .all(|t| *t != DataTypeName::Timestamptz)
        })
        .filter_map(|func| {
            let Some(lhs) = data_type_name_to_ast_data_type(&func.inputs_type[0]) else {
                return None;
            };
            let Some(rhs) = data_type_name_to_ast_data_type(&func.inputs_type[1]) else {
                return None;
            };
            let args = (lhs, rhs);
            let Some(op) = expr_type_to_inequality_op(func.func) else {
                return None;
            };
            Some((args, op))
        })
        .for_each(|(args, op)| funcs.entry(args).or_default().push(op));
    funcs
});
