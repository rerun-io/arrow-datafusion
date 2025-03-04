// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

pub mod aggregate;
pub mod array_expressions;
pub mod conditional_expressions;
#[cfg(feature = "crypto_expressions")]
pub mod crypto_expressions;
pub mod datetime_expressions;
pub mod execution_props;
pub mod expressions;
pub mod functions;
pub mod math_expressions;
mod physical_expr;
pub mod planner;
#[cfg(feature = "regex_expressions")]
pub mod regex_expressions;
mod scalar_function;
mod sort_expr;
pub mod string_expressions;
pub mod struct_expressions;
pub mod type_coercion;
pub mod udf;
#[cfg(feature = "unicode_expressions")]
pub mod unicode_expressions;
pub mod var_provider;
pub mod window;

// reexport this to maintain compatibility with anything that used from_slice previously
pub use aggregate::AggregateExpr;
pub use datafusion_common::from_slice;
pub use physical_expr::{ExprBoundaries, PhysicalExpr, PhysicalExprStats};
pub use planner::create_physical_expr;
pub use scalar_function::ScalarFunctionExpr;
pub use sort_expr::PhysicalSortExpr;
