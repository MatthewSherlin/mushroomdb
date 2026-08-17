use core_storage::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    columns: Vec<String>,
    rows: Vec<Vec<Option<Value>>>,
}

impl ResultSet {
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    pub fn push_row(&mut self, row: Vec<Option<Value>>) {
        assert_eq!(
            row.len(),
            self.columns.len(),
            "row arity does not match ResultSet columns"
        );
        self.rows.push(row);
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// # Panics — if `i >= self.len()`.
    pub fn row(&self, i: usize) -> &[Option<Value>] {
        &self.rows[i]
    }

    pub fn get(&self, i: usize, col: &str) -> Option<&Value> {
        let idx = self.columns.iter().position(|c| c == col)?;
        self.rows.get(i)?.get(idx)?.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::ResultSet;
    use core_storage::Value;

    #[test]
    fn resultset_basics() {
        let mut rs = ResultSet::new(vec!["a".into(), "b".into()]);
        rs.push_row(vec![Some(Value::Int(1)), None]);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs.get(0, "a"), Some(&Value::Int(1)));
        assert_eq!(rs.get(0, "b"), None);
        assert_eq!(rs.get(0, "zz"), None);
        assert_eq!(rs.row(0)[1], None);
    }

    #[test]
    #[should_panic]
    fn resultset_arity_mismatch_panics() {
        let mut rs = ResultSet::new(vec!["a".into()]);
        rs.push_row(vec![]);
    }

    #[test]
    fn resultset_empty_and_columns() {
        let rs = ResultSet::new(vec!["a".into()]);
        assert!(rs.is_empty());
        assert_eq!(rs.len(), 0);
        assert_eq!(rs.columns(), &["a".to_string()]);
    }
}
