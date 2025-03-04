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

//! Literal expressions for physical operations

use std::any::Any;
use std::sync::Arc;

use arrow::{
    datatypes::{DataType, Schema},
    record_batch::RecordBatch,
};

use crate::{ExprBoundaries, PhysicalExpr, PhysicalExprStats};
use datafusion_common::ScalarValue;
use datafusion_common::{ColumnStatistics, Result};
use datafusion_expr::{ColumnarValue, Expr};

/// Represents a literal value
#[derive(Debug)]
pub struct Literal {
    value: ScalarValue,
}

impl Literal {
    /// Create a literal value expression
    pub fn new(value: ScalarValue) -> Self {
        Self { value }
    }

    /// Get the scalar value
    pub fn value(&self) -> &ScalarValue {
        &self.value
    }
}

impl std::fmt::Display for Literal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.value)
    }
}

impl PhysicalExpr for Literal {
    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(self.value.get_datatype())
    }

    fn nullable(&self, _input_schema: &Schema) -> Result<bool> {
        Ok(self.value.is_null())
    }

    fn evaluate(&self, _batch: &RecordBatch) -> Result<ColumnarValue> {
        Ok(ColumnarValue::Scalar(self.value.clone()))
    }

    fn expr_stats(&self) -> Arc<dyn PhysicalExprStats> {
        Arc::new(LiteralExprStats {
            value: self.value.clone(),
        })
    }
}

struct LiteralExprStats {
    value: ScalarValue,
}

impl PhysicalExprStats for LiteralExprStats {
    #[allow(unused_variables)]
    /// A literal's boundaries are the same as its value's boundaries (since it is a
    /// scalar, both min/max are the same).
    fn boundaries(&self, columns: &[ColumnStatistics]) -> Option<ExprBoundaries> {
        Some(ExprBoundaries::new(
            self.value.clone(),
            self.value.clone(),
            Some(1),
        ))
    }
}

/// Create a literal expression
pub fn lit<T: datafusion_expr::Literal>(value: T) -> Arc<dyn PhysicalExpr> {
    match value.lit() {
        Expr::Literal(v) => Arc::new(Literal::new(v)),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int32Array;
    use arrow::datatypes::*;
    use datafusion_common::Result;

    #[test]
    fn literal_i32() -> Result<()> {
        // create an arbitrary record bacth
        let schema = Schema::new(vec![Field::new("a", DataType::Int32, true)]);
        let a = Int32Array::from(vec![Some(1), None, Some(3), Some(4), Some(5)]);
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a)])?;

        // create and evaluate a literal expression
        let literal_expr = lit(42i32);
        assert_eq!("42", format!("{}", literal_expr));

        let literal_array = literal_expr.evaluate(&batch)?.into_array(batch.num_rows());
        let literal_array = literal_array.as_any().downcast_ref::<Int32Array>().unwrap();

        // note that the contents of the literal array are unrelated to the batch contents except for the length of the array
        assert_eq!(literal_array.len(), 5); // 5 rows in the batch
        for i in 0..literal_array.len() {
            assert_eq!(literal_array.value(i), 42);
        }

        Ok(())
    }

    #[test]
    fn literal_stats() -> Result<()> {
        let literal_expr = lit(42i32);
        let stats = literal_expr.expr_stats();
        let boundaries = stats.boundaries(&[]).unwrap();
        assert_eq!(boundaries.min_value, ScalarValue::Int32(Some(42)));
        assert_eq!(boundaries.max_value, ScalarValue::Int32(Some(42)));
        assert_eq!(boundaries.distinct_count, Some(1));
        assert_eq!(boundaries.selectivity, None);

        Ok(())
    }
}
