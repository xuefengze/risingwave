// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use risingwave_common::error::{ErrorCode, Result};
use risingwave_common::types::DataType;
use risingwave_sqlparser::ast::Ident;

use crate::binder::Binder;
use crate::expr::{CorrelatedInputRef, ExprImpl, ExprType, FunctionCall, InputRef, Literal};

impl Binder {
    pub fn bind_column(&mut self, idents: &[Ident]) -> Result<ExprImpl> {
        // TODO: check quote style of `ident`.
        let (_schema_name, table_name, column_name) = match idents {
            [column] => (None, None, column.real_value()),
            [table, column] => (None, Some(table.real_value()), column.real_value()),
            [schema, table, column] => (
                Some(schema.real_value()),
                Some(table.real_value()),
                column.real_value(),
            ),
            _ => {
                return Err(
                    ErrorCode::InternalError(format!("Too many idents: {:?}", idents)).into(),
                )
            }
        };

        match self
            .context
            .get_column_binding_indices(&table_name, &column_name)
        {
            Ok(mut indices) => {
                match indices.len() {
                    0 => unreachable!(),
                    1 => {
                        let index = indices[0];
                        let column = &self.context.columns[index];
                        return Ok(
                            InputRef::new(column.index, column.field.data_type.clone()).into()
                        );
                    }
                    _ => {
                        indices.sort(); // make sure we have a consistent result
                        let inputs = indices
                            .iter()
                            .map(|index| {
                                let column = &self.context.columns[*index];
                                InputRef::new(column.index, column.field.data_type.clone()).into()
                            })
                            .collect::<Vec<_>>();
                        return Ok(FunctionCall::new(ExprType::Coalesce, inputs)?.into());
                    }
                }
            }
            Err(e) => {
                // If the error message is not that the column is not found, throw the error
                if let ErrorCode::ItemNotFound(_) = e {
                } else {
                    return Err(e.into());
                }
            }
        }

        // Try to find a correlated column in `upper_contexts`, starting from the innermost context.
        let mut err = ErrorCode::ItemNotFound(format!("Invalid column: {}", column_name));

        for (i, lateral_context) in self.lateral_contexts.iter().rev().enumerate() {
            if lateral_context.is_visible {
                let context = &lateral_context.context;
                // input ref from lateral context `depth` starts from 1.
                let depth = i + 1;
                match context.get_column_binding_index(&table_name, &column_name) {
                    Ok(index) => {
                        let column = &context.columns[index];
                        return Ok(CorrelatedInputRef::new(
                            column.index,
                            column.field.data_type.clone(),
                            depth,
                        )
                        .into());
                    }
                    Err(e) => {
                        err = e;
                    }
                }
            }
        }

        for (i, (context, lateral_contexts)) in
            self.upper_subquery_contexts.iter().rev().enumerate()
        {
            // `depth` starts from 1.
            let depth = i + 1;
            match context.get_column_binding_index(&table_name, &column_name) {
                Ok(index) => {
                    let column = &context.columns[index];
                    return Ok(CorrelatedInputRef::new(
                        column.index,
                        column.field.data_type.clone(),
                        depth,
                    )
                    .into());
                }
                Err(e) => {
                    err = e;
                }
            }

            for (i, lateral_context) in lateral_contexts.iter().rev().enumerate() {
                if lateral_context.is_visible {
                    let context = &lateral_context.context;
                    // correlated input ref from lateral context `depth` starts from 1.
                    let depth = i + 1;
                    match context.get_column_binding_index(&table_name, &column_name) {
                        Ok(index) => {
                            let column = &context.columns[index];
                            return Ok(CorrelatedInputRef::new(
                                column.index,
                                column.field.data_type.clone(),
                                depth,
                            )
                            .into());
                        }
                        Err(e) => {
                            err = e;
                        }
                    }
                }
            }
        }
        // `CTID` is a system column in postgres.
        // https://www.postgresql.org/docs/current/ddl-system-columns.html
        //
        // We return an empty string here to support some tools such as DataGrip.
        //
        // FIXME: The type of `CTID` should be `tid`.
        // FIXME: The `CTID` column should be unique, so literal may break something.
        // FIXME: At least we should add a notice here.
        if let ErrorCode::ItemNotFound(_) = err
            && column_name == "ctid"
        {
            return Ok(Literal::new(Some("".into()), DataType::Varchar).into());
        }
        Err(err.into())
    }
}
