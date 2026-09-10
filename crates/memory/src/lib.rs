use lbug::{Connection, Database, Error, SystemConfig};
use std::{fs, path::Path};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const EXPECTED_SCHEMA_VERSION: &str = "1";

pub struct MemoryDb {
    db: Database,
}

impl MemoryDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let db = Database::new(path.to_string_lossy().as_ref(), SystemConfig::default())?;

        Ok(Self { db })
    }

    fn connection(&self) -> Result<Connection<'_>> {
        Ok(Connection::new(&self.db)?)
    }

    pub fn initialize_schema(&self) -> Result<()> {
        let conn = self.connection()?;
        let schema = include_str!("../../../schemas/memory.cypher");

        for statement in schema
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
        {
            conn.query(statement)?;
        }

        drop(conn);
        self.ensure_schema_version()?;

        Ok(())
    }

    fn ensure_schema_version(&self) -> Result<()> {
        let conn = self.connection()?;
        let mut result = conn.query(
            "MATCH (m:SystemMeta {key: 'schema_version'})
             RETURN m.value",
        )?;

        match result.next() {
            Some(row) => {
                let version = match row.first() {
                    Some(lbug::Value::String(value)) => value.as_str(),
                    Some(value) => {
                        return Err(format!(
                            "Invalid schema version value in SystemMeta: {value:?}"
                        )
                        .into());
                    }
                    None => {
                        return Err("Schema version value is missing.".into());
                    }
                };

                if version != EXPECTED_SCHEMA_VERSION {
                    return Err(format!(
                        "Unsupported schema version: {version}, expected {EXPECTED_SCHEMA_VERSION}"
                    )
                    .into());
                }
            }
            None => {
                conn.query(
                    "CREATE (:SystemMeta {
                        key: 'schema_version',
                        value: '1'
                    })",
                )?;
            }
        }

        Ok(())
    }

    pub fn schema_version(&self) -> Result<String> {
        let conn = self.connection()?;
        let mut result = conn.query(
            "MATCH (m:SystemMeta {key: 'schema_version'})
             RETURN m.value",
        )?;

        let Some(row) = result.next() else {
            return Err("Schema version metadata is missing.".into());
        };

        match row.first() {
            Some(lbug::Value::String(value)) => Ok(value.clone()),
            Some(value) => Err(format!(
                "Invalid schema version value in SystemMeta: {value:?}"
            )
            .into()),
            None => Err("Schema version value is missing.".into()),
        }
    }

    pub fn verify_basic_schema(&self) -> Result<()> {
        let conn = self.connection()?;

        let mut result = conn.query(
            "MATCH (m:SystemMeta {key: 'schema_version'})
             RETURN m.key, m.value",
        )?;

        while let Some(row) = result.next() {
            println!("SystemMeta: {row:?}");
        }

        Ok(())
    }

    pub fn _ladybug_error_type_name(_: &Error) -> &'static str {
        "lbug::Error"
    }
}
