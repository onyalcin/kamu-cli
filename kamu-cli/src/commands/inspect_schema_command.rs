use super::{CLIError, Command};

use datafusion::parquet::schema::types::Type;
use kamu::domain::*;
use opendatafabric::*;
use std::sync::Arc;

pub struct InspectSchemaCommand {
    query_svc: Arc<dyn QueryService>,
    dataset_id: DatasetIDBuf,
    output_format: Option<String>,
}

impl InspectSchemaCommand {
    pub fn new(
        query_svc: Arc<dyn QueryService>,
        dataset_id: DatasetIDBuf,
        output_format: Option<&str>,
    ) -> Self {
        Self {
            query_svc,
            dataset_id,
            output_format: output_format.map(|s| s.to_owned()),
        }
    }

    fn print_schema_ddl(&self, schema: &Type) {
        match schema {
            Type::GroupType { fields, .. } => {
                for field in fields {
                    self.print_schema_ddl_rec(field, 0);
                }
            }
            _ => unreachable!(),
        }
    }

    // TODO: This produces DDL that doesn't match the SQL types we use elsewhere
    fn print_schema_ddl_rec(&self, t: &Type, depth: i32) {
        use datafusion::parquet::basic::ConvertedType;
        use datafusion::parquet::basic::Type as BasicType;

        self.indent(depth);

        match t {
            Type::PrimitiveType {
                basic_info,
                physical_type,
                scale,
                precision,
                ..
            } => {
                print!("{} ", console::style(basic_info.name()).bold());
                let typ = if *precision > 0 {
                    format!("DECIMAL({}, {})", precision, scale)
                } else if basic_info.converted_type() == ConvertedType::UTF8 {
                    format!("STRING")
                } else if basic_info.converted_type() != ConvertedType::NONE {
                    format!("{:?}", basic_info.converted_type())
                } else if *physical_type == BasicType::INT96 {
                    format!("TIMESTAMP")
                } else {
                    format!("{:?}", physical_type)
                };
                print!("{}", console::style(typ).cyan());
            }
            Type::GroupType { basic_info, fields } => {
                print!("{} ", console::style(basic_info.name()).bold());
                println!("{}", console::style("STRUCT<").cyan());
                for field in fields {
                    self.print_schema_ddl_rec(field, depth + 1);
                }
                self.indent(depth);
                print!("{}", console::style(">").cyan());
            }
        }
        println!("{}", console::style(",").dim());
    }

    fn indent(&self, depth: i32) {
        for _ in 0..depth {
            print!("  ");
        }
    }

    fn print_schema_parquet(&self, schema: &Type) {
        datafusion::parquet::schema::printer::print_schema(&mut std::io::stdout(), &schema);
    }
}

impl Command for InspectSchemaCommand {
    fn run(&mut self) -> Result<(), CLIError> {
        let schema = self
            .query_svc
            .get_schema(&self.dataset_id)
            .map_err(|e| match e {
                QueryError::DomainError(e) => CLIError::usage_error_from(e),
                e @ QueryError::DataFusionError(_) => CLIError::failure(e),
                e @ QueryError::InternalError(_) => CLIError::failure(e),
            })?;

        match self.output_format.as_ref().map(|s| s.as_str()) {
            None | Some("ddl") => self.print_schema_ddl(&schema),
            Some("parquet") => self.print_schema_parquet(&schema),
            _ => unreachable!(),
        }

        Ok(())
    }
}