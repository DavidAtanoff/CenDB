//! System catalog: metadata about tables, columns, indexes, views, and
//! stored procedures.

use std::collections::HashMap;

/// Information about a table.
#[derive(Clone, Debug)]
pub struct TableInfo {
    pub name: String,
    pub model: String, // "relational", "kv", "document", "timeseries", "graph"
    pub row_count: u64,
    pub columns: Vec<ColumnInfo>,
    pub created_at: u64,
}

/// Information about a column.
#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub is_indexed: bool,
}

/// Information about an index.
#[derive(Clone, Debug)]
pub struct IndexInfo {
    pub name: String,
    pub table: String,
    pub column: String,
    pub index_type: String, // "art", "secondary", "fts", "spatial"
    pub entry_count: u64,
}

/// Information about a view.
#[derive(Clone, Debug)]
pub struct ViewInfo {
    pub name: String,
    /// The CenQL pipeline that defines the view.
    pub definition: String,
    pub created_at: u64,
}

/// Information about a stored procedure.
#[derive(Clone, Debug)]
pub struct StoredProcedure {
    pub name: String,
    /// The CenQL statement(s) that make up the procedure body.
    pub body: String,
    /// Parameter names (in order).
    pub params: Vec<String>,
    pub created_at: u64,
}

/// The system catalog: a registry of all tables, columns, indexes,
/// views, and stored procedures. Persists to disk as JSON.
pub struct SystemCatalog {
    tables: HashMap<String, TableInfo>,
    indexes: Vec<IndexInfo>,
    views: HashMap<String, ViewInfo>,
    procedures: HashMap<String, StoredProcedure>,
}

impl SystemCatalog {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            indexes: Vec::new(),
            views: HashMap::new(),
            procedures: HashMap::new(),
        }
    }

    // ========================================================================
    // Table operations.
    // ========================================================================

    /// Register a table.
    pub fn register_table(&mut self, info: TableInfo) {
        self.tables.insert(info.name.clone(), info);
    }

    /// Look up a table by name.
    pub fn get_table(&self, name: &str) -> Option<&TableInfo> {
        self.tables.get(name)
    }

    /// List all tables (simulates `SELECT * FROM __tables`).
    pub fn list_tables(&self) -> Vec<&TableInfo> {
        self.tables.values().collect()
    }

    /// Drop a table from the catalog.
    pub fn drop_table(&mut self, name: &str) -> bool {
        let removed = self.tables.remove(name).is_some();
        if removed {
            // Also remove indexes on this table.
            self.indexes.retain(|i| i.table != name);
        }
        removed
    }

    /// Add a column to an existing table (schema evolution).
    pub fn add_column(&mut self, table: &str, col: ColumnInfo) -> Result<(), String> {
        let info = self.tables.get_mut(table).ok_or("table not found")?;
        info.columns.push(col);
        Ok(())
    }

    /// Drop a column from a table (schema evolution).
    pub fn drop_column(&mut self, table: &str, col_name: &str) -> Result<(), String> {
        let info = self.tables.get_mut(table).ok_or("table not found")?;
        info.columns.retain(|c| c.name != col_name);
        Ok(())
    }

    /// Rename a column (schema evolution).
    pub fn rename_column(&mut self, table: &str, old_name: &str, new_name: &str) -> Result<(), String> {
        let info = self.tables.get_mut(table).ok_or("table not found")?;
        for col in &mut info.columns {
            if col.name == old_name {
                col.name = new_name.to_string();
                return Ok(());
            }
        }
        Err("column not found".to_string())
    }

    // ========================================================================
    // Index operations.
    // ========================================================================

    /// Register an index.
    pub fn register_index(&mut self, info: IndexInfo) {
        self.indexes.push(info);
    }

    /// List all indexes (simulates `SELECT * FROM __indexes`).
    pub fn list_indexes(&self) -> &[IndexInfo] {
        &self.indexes
    }

    /// Drop an index by name.
    pub fn drop_index(&mut self, name: &str) -> bool {
        let before = self.indexes.len();
        self.indexes.retain(|i| i.name != name);
        self.indexes.len() < before
    }

    /// Find indexes on a specific table.
    pub fn indexes_for_table(&self, table: &str) -> Vec<&IndexInfo> {
        self.indexes.iter().filter(|i| i.table == table).collect()
    }

    // ========================================================================
    // View operations.
    // ========================================================================

    /// Create a view.
    pub fn create_view(&mut self, info: ViewInfo) {
        self.views.insert(info.name.clone(), info);
    }

    /// Look up a view by name.
    pub fn get_view(&self, name: &str) -> Option<&ViewInfo> {
        self.views.get(name)
    }

    /// List all views.
    pub fn list_views(&self) -> Vec<&ViewInfo> {
        self.views.values().collect()
    }

    /// Drop a view.
    pub fn drop_view(&mut self, name: &str) -> bool {
        self.views.remove(name).is_some()
    }

    // ========================================================================
    // Stored procedure operations.
    // ========================================================================

    /// Create a stored procedure.
    pub fn create_procedure(&mut self, proc: StoredProcedure) {
        self.procedures.insert(proc.name.clone(), proc);
    }

    /// Look up a stored procedure by name.
    pub fn get_procedure(&self, name: &str) -> Option<&StoredProcedure> {
        self.procedures.get(name)
    }

    /// List all stored procedures.
    pub fn list_procedures(&self) -> Vec<&StoredProcedure> {
        self.procedures.values().collect()
    }

    /// Drop a stored procedure.
    pub fn drop_procedure(&mut self, name: &str) -> bool {
        self.procedures.remove(name).is_some()
    }

    // ========================================================================
    // Persistence.
    // ========================================================================

    /// Serialize the catalog to JSON for persistence.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"tables\":[");
        for (i, (_, t)) in self.tables.iter().enumerate() {
            if i > 0 { out.push(','); }
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"model\":\"{}\",\"row_count\":{},\"created_at\":{}}}",
                t.name, t.model, t.row_count, t.created_at
            ));
        }
        out.push_str("],\"indexes\":[");
        for (i, idx) in self.indexes.iter().enumerate() {
            if i > 0 { out.push(','); }
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"table\":\"{}\",\"column\":\"{}\",\"type\":\"{}\",\"count\":{}}}",
                idx.name, idx.table, idx.column, idx.index_type, idx.entry_count
            ));
        }
        out.push_str("],\"views\":[");
        for (i, (_, v)) in self.views.iter().enumerate() {
            if i > 0 { out.push(','); }
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"definition\":\"{}\",\"created_at\":{}}}",
                v.name, v.definition.replace('"', "\\\""), v.created_at
            ));
        }
        out.push_str("],\"procedures\":[");
        for (i, (_, p)) in self.procedures.iter().enumerate() {
            if i > 0 { out.push(','); }
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"params\":{:?},\"created_at\":{}}}",
                p.name, p.params, p.created_at
            ));
        }
        out.push_str("]}");
        out
    }

    /// Save the catalog to a file on disk.
    pub fn save_to_file(&self, path: &str) -> std::io::Result<()> {
        std::fs::write(path, self.to_json())
    }

    // ========================================================================
    // Stats.
    // ========================================================================

    /// Total number of tables.
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Total number of indexes.
    pub fn index_count(&self) -> usize {
        self.indexes.len()
    }

    /// Total number of views.
    pub fn view_count(&self) -> usize {
        self.views.len()
    }

    /// Total number of stored procedures.
    pub fn procedure_count(&self) -> usize {
        self.procedures.len()
    }
}

impl Default for SystemCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_basic() {
        let mut cat = SystemCatalog::new();
        cat.register_table(TableInfo {
            name: "users".to_string(),
            model: "relational".to_string(),
            row_count: 1000,
            columns: vec![
                ColumnInfo { name: "id".into(), data_type: "i64".into(), nullable: false, is_primary_key: true, is_indexed: true },
                ColumnInfo { name: "name".into(), data_type: "bytes".into(), nullable: false, is_primary_key: false, is_indexed: false },
            ],
            created_at: 1000,
        });

        assert_eq!(cat.list_tables().len(), 1);
        assert!(cat.get_table("users").is_some());

        // Schema evolution: add column.
        cat.add_column("users", ColumnInfo {
            name: "email".into(),
            data_type: "bytes".into(),
            nullable: true,
            is_primary_key: false,
            is_indexed: false,
        }).unwrap();
        assert_eq!(cat.get_table("users").unwrap().columns.len(), 3);

        // Drop column.
        cat.drop_column("users", "email").unwrap();
        assert_eq!(cat.get_table("users").unwrap().columns.len(), 2);
    }

    #[test]
    fn index_registry() {
        let mut cat = SystemCatalog::new();
        cat.register_index(IndexInfo {
            name: "idx_users_name".into(),
            table: "users".into(),
            column: "name".into(),
            index_type: "secondary".into(),
            entry_count: 1000,
        });
        assert_eq!(cat.list_indexes().len(), 1);
    }

    #[test]
    fn drop_index() {
        let mut cat = SystemCatalog::new();
        cat.register_index(IndexInfo {
            name: "idx1".into(), table: "t".into(), column: "c".into(),
            index_type: "art".into(), entry_count: 0,
        });
        cat.register_index(IndexInfo {
            name: "idx2".into(), table: "t".into(), column: "d".into(),
            index_type: "art".into(), entry_count: 0,
        });
        assert_eq!(cat.index_count(), 2);
        assert!(cat.drop_index("idx1"));
        assert_eq!(cat.index_count(), 1);
        assert!(!cat.drop_index("nonexistent"));
    }

    #[test]
    fn indexes_for_table() {
        let mut cat = SystemCatalog::new();
        cat.register_index(IndexInfo {
            name: "idx1".into(), table: "users".into(), column: "name".into(),
            index_type: "art".into(), entry_count: 0,
        });
        cat.register_index(IndexInfo {
            name: "idx2".into(), table: "orders".into(), column: "id".into(),
            index_type: "art".into(), entry_count: 0,
        });
        assert_eq!(cat.indexes_for_table("users").len(), 1);
        assert_eq!(cat.indexes_for_table("orders").len(), 1);
        assert_eq!(cat.indexes_for_table("nonexistent").len(), 0);
    }

    #[test]
    fn view_operations() {
        let mut cat = SystemCatalog::new();
        cat.create_view(ViewInfo {
            name: "active_users".into(),
            definition: r#"from users | filter status == "active""#.into(),
            created_at: 1000,
        });
        assert_eq!(cat.view_count(), 1);
        assert!(cat.get_view("active_users").is_some());
        assert!(cat.get_view("nonexistent").is_none());
        assert_eq!(cat.list_views().len(), 1);
        assert!(cat.drop_view("active_users"));
        assert_eq!(cat.view_count(), 0);
    }

    #[test]
    fn stored_procedure_operations() {
        let mut cat = SystemCatalog::new();
        cat.create_procedure(StoredProcedure {
            name: "get_user".into(),
            body: "from users | filter id == $user_id".into(),
            params: vec!["user_id".into()],
            created_at: 1000,
        });
        assert_eq!(cat.procedure_count(), 1);
        assert!(cat.get_procedure("get_user").is_some());
        let proc = cat.get_procedure("get_user").unwrap();
        assert_eq!(proc.params, vec!["user_id".to_string()]);
        assert!(cat.drop_procedure("get_user"));
        assert_eq!(cat.procedure_count(), 0);
    }

    #[test]
    fn drop_table_also_drops_indexes() {
        let mut cat = SystemCatalog::new();
        cat.register_table(TableInfo {
            name: "users".into(), model: "relational".into(),
            row_count: 0, columns: vec![], created_at: 0,
        });
        cat.register_index(IndexInfo {
            name: "idx1".into(), table: "users".into(), column: "name".into(),
            index_type: "art".into(), entry_count: 0,
        });
        cat.register_index(IndexInfo {
            name: "idx2".into(), table: "orders".into(), column: "id".into(),
            index_type: "art".into(), entry_count: 0,
        });
        assert_eq!(cat.index_count(), 2);
        cat.drop_table("users");
        assert_eq!(cat.index_count(), 1); // only orders index remains
    }

    #[test]
    fn catalog_json_serialization() {
        let mut cat = SystemCatalog::new();
        cat.register_table(TableInfo {
            name: "users".into(), model: "relational".into(),
            row_count: 100, columns: vec![], created_at: 1000,
        });
        cat.create_view(ViewInfo {
            name: "v".into(), definition: "from users".into(), created_at: 2000,
        });
        let json = cat.to_json();
        assert!(json.contains("users"));
        assert!(json.contains("relational"));
        assert!(json.contains("\"v\""));
    }

    #[test]
    fn catalog_save_to_file() {
        let mut cat = SystemCatalog::new();
        cat.register_table(TableInfo {
            name: "t".into(), model: "kv".into(),
            row_count: 0, columns: vec![], created_at: 0,
        });
        let path = std::env::temp_dir().join(format!("cendb_catalog_test_{}.json", std::process::id()));
        cat.save_to_file(path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"t\""));
        std::fs::remove_file(&path).ok();
    }
}
