use std::error;
use std::iter;
use std::path;


use reqwest;
use sqlite;


const SCHEMA_SQL: &str = "
    CREATE TABLE urls (
    	url TEXT NOT NULL UNIQUE,
    	path TEXT NOT NULL,
    	last_modified TEXT,
    	etag TEXT
    );
";


/// All the information we have about a given URL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CacheRecord {
    /// The path to the cached response body on disk.
    pub path: String,
    /// The value of the Last-Modified header in the original response.
    pub last_modified: Option<reqwest::header::HttpDate>,
    /// The value of the Etag header in the original response.
    pub etag: Option<String>,
}


/// Represents the rows returned by a query.
struct Rows<'a>(sqlite::Cursor<'a>);


impl<'a> iter::Iterator for Rows<'a> {
    type Item = Vec<sqlite::Value>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
            .unwrap_or_else(|err| {
                warn!("Failed to get next row from SQLite: {}", err);
                None
            })
            .map(|values| values.to_vec())
    }
}


/// Represents an attempt to record information in the database.
#[must_use]
pub struct Transaction<'a> {
    conn: &'a sqlite::Connection,
    committed: bool,
}

impl<'a> Transaction<'a> {
    fn new(conn: &'a sqlite::Connection) -> Transaction<'a> {
        Transaction{conn: conn, committed: false}
    }

    fn commit(mut self) -> Result<(), Box<error::Error>> {
        println!("Attempting to commit changes...");
        self.committed = true;

        self.conn.execute("COMMIT;").map_err(|err| {
            println!("Failed to commit changes: {}", err);
            match self.conn.execute("ROLLBACK;") {
                // Rollback worked, return the original error
                Ok(_) => err,
                // Rollback failed too! Let's warn about that,
                // but return the original error.
                Err(new_err) => {
                    println!("Failed to rollback too! {}", new_err);
                    err
                },
            }
        })?;
        println!("Commit successful!");
        Ok(())
    }
}

impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        if self.committed {
            println!("Changes already committed, nothing to do.")
        } else {
            println!("Attempting to rollback changes...");
            self.conn.execute("ROLLBACK;").unwrap_or_else(|err| {
                println!("Failed to rollback changes: {}", err)
            })
        }
    }
}

/// Represents the database that describes the contents of the cache.
pub struct CacheDB(sqlite::Connection);

impl CacheDB {
    /// Create a cache database in the given file.
    pub fn new<P: AsRef<path::Path>>(path: P)
        -> Result<CacheDB, Box<error::Error>>
    {
        // Package up the return value first, so we can use .query()
        // instead of wrangling sqlite directly.
        let res = CacheDB(sqlite::Connection::open(path)?);

        let rows: Vec<_> = res.query(
            "SELECT COUNT(*) FROM sqlite_master;",
            &[],
        )?.collect();
        if let sqlite::Value::Integer(0) = rows[0][0] {
            // No tables define in this DB, let's load our schema.
            res.0.execute(SCHEMA_SQL)?
        }

        Ok(res)
    }

    fn query<'a, T: AsRef<str>>(
        &'a self,
        query: T,
        params: &[sqlite::Value],
    ) -> sqlite::Result<Rows> {
        let mut cur = self.0.prepare(query)?.cursor();
        cur.bind(params)?;

        Ok(Rows(cur))
    }

    /// Return what the DB knows about a URL, if anything.
    pub fn get(&self, mut url: reqwest::Url)
        -> Result<CacheRecord, Box<error::Error>>
    {
        url.set_fragment(None);

        let mut rows = self.query("
            SELECT path, last_modified, etag
            FROM urls
            WHERE url = ?1
            ",
            &[sqlite::Value::String(url.as_str().into())],
        )?;

        rows.next()
            .map_or(
                Err(format!("URL not found in cache: {:?}", url)),
                |x| Ok(x),
            )
            .map(|row| -> Result<CacheRecord, Box<error::Error>> {
                let mut cols = row.into_iter();

                let path = match cols.next().unwrap() {
                    sqlite::Value::String(s) => Ok(s),
                    other => Err(format!("Path had wrong type: {:?}", other)),
                }?;

                let last_modified = match cols.next().unwrap() {
                    sqlite::Value::String(s) => {
                        use std::str::FromStr;
                        Some(reqwest::header::HttpDate::from_str(&s)?)
                    },
                    sqlite::Value::Null => { None },
                    other => {
                        warn!(
                            "last_modified contained weird type: {:?}",
                            other,
                        );
                        None
                    },
                };

                let etag = match cols.next().unwrap() {
                    sqlite::Value::String(s) => { Some(s) },
                    sqlite::Value::Null => { None },
                    other => {
                        warn!(
                            "last_modified contained weird type: {:?}",
                            other,
                        );
                        None
                    },
                };

                Ok(CacheRecord{path, last_modified, etag})
            })?
    }

    /// Record information about this information in the database.
    pub fn set(&mut self, mut url: reqwest::Url, record: CacheRecord)
        -> Result<Transaction, Box<error::Error>>
    {
        url.set_fragment(None);

        // Start a new transaction...
        self.0.execute("BEGIN;")?;

        // ...and immediately construct the value that will clean up
        // the transaction when necessary.
        let res = Transaction::new(&self.0);

        let rows = self.query("
            INSERT OR REPLACE INTO urls
                (url, path, last_modified, etag)
            VALUES
                (?1, ?2, ?3, ?4);
            ",
            &[
                sqlite::Value::String(url.as_str().into()),
                sqlite::Value::String(record.path),
                record.last_modified
                    .map(|date| {
                        sqlite::Value::String(format!("{}", date))
                    })
                    .unwrap_or(sqlite::Value::Null),
                record.etag
                    .map(|etag| { sqlite::Value::String(etag) })
                    .unwrap_or(sqlite::Value::Null),
            ],
        )?;

        // Exhaust the row iterator to ensure the query is executed.
        for _ in rows {};

        Ok(res)
    }
}


#[cfg(test)]
mod tests {
    extern crate tempdir;
    use reqwest;
    use sqlite;

    #[test]
    fn create_fresh_db() {
        let db = super::CacheDB::new(":memory:").unwrap();

        let rows: Vec<_> = db.query(
            "SELECT name FROM sqlite_master WHERE TYPE = ?1",
            &[sqlite::Value::String("table".into())],
        ).unwrap().collect();

        assert_eq!(rows, vec![vec![sqlite::Value::String("urls".into())]]);

    }

    #[test]
    fn reopen_existing_db() {
        let root = tempdir::TempDir::new("cachedb-test").unwrap().into_path();
        let db_path = root.join("cache.db");

        let db1 = super::CacheDB::new(&db_path).unwrap();
        let rows: Vec<_> = db1.query(
            "SELECT name FROM sqlite_master WHERE TYPE = ?1",
            &[sqlite::Value::String("table".into())],
        ).unwrap().collect();
        assert_eq!(rows, vec![vec![sqlite::Value::String("urls".into())]]);


        let db2 = super::CacheDB::new(&db_path).unwrap();
        let rows: Vec<_> = db2.query(
            "SELECT name FROM sqlite_master WHERE TYPE = ?1",
            &[sqlite::Value::String("table".into())],
        ).unwrap().collect();
        assert_eq!(rows, vec![vec![sqlite::Value::String("urls".into())]]);
    }

    #[test]
    fn open_bogus_db() {
        let res = super::CacheDB::new("does/not/exist");

        assert_eq!(res.is_err(), true);
    }

    #[test]
    fn get_from_empty_db() {
        let db = super::CacheDB::new(":memory:").unwrap();

        let err = db.get("http://example.com/".parse().unwrap()).unwrap_err();

        assert_eq!(
            err.description(),
            "URL not found in cache: \"http://example.com/\""
        );
    }

    #[test]
    fn get_unknown_url() {
        let db = super::CacheDB::new(":memory:").unwrap();

        db.0.execute("
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                )
            VALUES
                ( 'http://example.com/one'
                , 'path/to/data'
                , NULL
                , NULL
                )
            ;
        ").unwrap();

        let err = db.get(
            "http://example.com/two".parse().unwrap()
        ).unwrap_err();

        assert_eq!(
            err.description(),
            "URL not found in cache: \"http://example.com/two\""
        );
    }

    #[test]
    fn get_known_url() {
        let db = super::CacheDB::new(":memory:").unwrap();

        db.0.execute("
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                )
            VALUES
                ( 'http://example.com/'
                , 'path/to/data'
                , NULL
                , NULL
                )
            ;
        ").unwrap();

        let record = db.get(
            "http://example.com/".parse().unwrap()
        ).unwrap();

        assert_eq!(
            record,
            super::CacheRecord{
                path: "path/to/data".into(),
                last_modified: None,
                etag: None,
            }
        );
    }

    #[test]
    fn get_known_url_with_headers() {
        use std::str::FromStr;

        let db = super::CacheDB::new(":memory:").unwrap();
        db.0.execute("
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                )
            VALUES
                ( 'http://example.com/'
                , 'path/to/data'
                , 'Thu, 01 Jan 1970 00:00:00 GMT'
                , 'some-crazy-text'
                )
            ;
        ").unwrap();

        let record = db.get(
            "http://example.com/".parse().unwrap()
        ).unwrap();

        assert_eq!(
            record,
            super::CacheRecord{
                path: "path/to/data".into(),
                last_modified: Some(reqwest::header::HttpDate::from_str(
                    "Thu, 01 Jan 1970 00:00:00 GMT"
                ).unwrap()),
                etag: Some("some-crazy-text".into()),
            }
        );
    }

    #[test]
    fn get_url_with_invalid_path() {

        let db = super::CacheDB::new(":memory:").unwrap();

        db.0.execute("
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                )
            VALUES
                ( 'http://example.com/'
                , CAST('abc' AS BLOB)
                , NULL
                , NULL
                )
            ;
        ").unwrap();

        let err = db.get("http://example.com/".parse().unwrap()).unwrap_err();

        assert_eq!(
            err.description(),
            "Path had wrong type: Binary([97, 98, 99])"
        );
    }

    #[test]
    fn get_url_with_invalid_last_modified_and_etag() {

        let db = super::CacheDB::new(":memory:").unwrap();

        db.0.execute("
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                )
            VALUES
                ( 'http://example.com/'
                , 'path/to/data'
                , CAST('abc' AS BLOB)
                , CAST('def' AS BLOB)
                )
            ;
        ").unwrap();

        let record = db.get("http://example.com/".parse().unwrap()).unwrap();

        assert_eq!(
            record,
            super::CacheRecord{
                path: "path/to/data".into(),
                // We expect TEXT or NULL; if we get a BLOB value we
                // treat it as NULL.
                last_modified: None,
                etag: None,
            }
        );
    }

    #[test]
    fn get_ignores_fragments() {
        let db = super::CacheDB::new(":memory:").unwrap();

        db.0.execute("
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                )
            VALUES
                ( 'http://example.com/'
                , 'path/to/data'
                , NULL
                , NULL
                )
            ;
        ").unwrap();

        let record = db.get(
            "http://example.com/#top".parse().unwrap()
        ).unwrap();

        assert_eq!(
            record,
            super::CacheRecord{
                path: "path/to/data".into(),
                last_modified: None,
                etag: None,
            }
        );
    }

    #[test]
    fn insert_data_with_commit() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let record = super::CacheRecord{
            path: "path/to/data".into(),
            last_modified: None,
            etag: None,
        };

        let mut db = super::CacheDB::new(":memory:").unwrap();

        // Add data into the DB, inside a block so we can be sure all the
        //  intermediates have been dropped afterward.
        {
            let trans = db.set(url.clone(), record.clone()).unwrap();

            trans.commit().unwrap();
        }

        let rows: Vec<_> = db.query(
            "SELECT * FROM urls;",
            &[],
        ).unwrap().collect();
        println!("Table content: {:?}", rows);

        // Did our data make it into the DB?
        assert_eq!(db.get(url).unwrap(), record);
    }

    #[test]
    fn insert_data_with_all_fields() {
        use std::str::FromStr;

        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let record = super::CacheRecord{
            path: "path/to/data".into(),
            last_modified: Some(reqwest::header::HttpDate::from_str(
                "Thu, 01 Jan 1970 00:00:00 GMT"
            ).unwrap()),
            etag: Some("some-crazy-text".into()),
        };

        let mut db = super::CacheDB::new(":memory:").unwrap();

        // Add data into the DB, inside a block so we can be sure all the
        //  intermediates have been dropped afterward.
        db.set(url.clone(), record.clone()).unwrap().commit().unwrap();

        // Did our data make it into the DB?
        assert_eq!(db.get(url).unwrap(), record);
    }

    #[test]
    fn insert_data_without_commit() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let record = super::CacheRecord{
            path: "path/to/data".into(),
            last_modified: None,
            etag: None,
        };

        let mut db = super::CacheDB::new(":memory:").unwrap();

        // Add data into the DB, inside a block so we can be sure all the
        //  intermediates have been dropped afterward.
        {
            let _ = db.set(url.clone(), record.clone()).unwrap();

            // Don't commit before the end of the block!
        }

        // Did our data make it into the DB?
        assert_eq!(
            db.get(url).unwrap_err().description(),
            "URL not found in cache: \"http://example.com/\""
        );
    }

    #[test]
    fn overwrite_data() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();

        let record_one = super::CacheRecord{
            path: "path/to/data/one".into(),
            last_modified: None,
            etag: Some("one".into()),
        };

        let record_two = super::CacheRecord{
            path: "path/to/data/two".into(),
            last_modified: None,
            etag: Some("two".into()),
        };

        let mut db = super::CacheDB::new(":memory:").unwrap();

        // Our example URL just returned record one.
        db.set(url.clone(), record_one.clone()).unwrap().commit().unwrap();

        // We recorded that correctly, right?
        assert_eq!(
            db.get(url.clone()).unwrap(),
            record_one
        );

        // Oh, the URL got updated!
        db.set(url.clone(), record_two.clone()).unwrap().commit().unwrap();

        // We recorded that correctly too, right?
        assert_eq!(
            db.get(url.clone()).unwrap(),
            record_two
        );
    }

    #[test]
    fn insert_data_ignores_url_fragment() {
        let record_one = super::CacheRecord{
            path: "path/to/data/one".into(),
            last_modified: None,
            etag: Some("one".into()),
        };

        let record_two = super::CacheRecord{
            path: "path/to/data/two".into(),
            last_modified: None,
            etag: Some("two".into()),
        };

        let mut db = super::CacheDB::new(":memory:").unwrap();

        // Try to insert data with a fragment
        db.set(
            "http://example.com/#frag".parse().unwrap(),
            record_one.clone(),
        ).unwrap().commit().unwrap();

        // Try to insert different data without a fragment
        db.set(
            "http://example.com/".parse().unwrap(),
            record_two.clone(),
        ).unwrap().commit().unwrap();

        // Querying with any fragment, or without a fragment, will always
        // give us the same information.
        assert_eq!(
            db.get("http://example.com/#frag".parse().unwrap()).unwrap(),
            record_two
        );
        assert_eq!(
            db.get("http://example.com/#garf".parse().unwrap()).unwrap(),
            record_two
        );
        assert_eq!(
            db.get("http://example.com/".parse().unwrap()).unwrap(),
            record_two
        );

        // If we insert data with a fragment, the new data is returned for
        // all queries.
        db.set(
            "http://example.com/#boop".parse().unwrap(),
            record_one.clone(),
        ).unwrap().commit().unwrap();

        assert_eq!(
            db.get("http://example.com/#frag".parse().unwrap()).unwrap(),
            record_one
        );
        assert_eq!(
            db.get("http://example.com/#garf".parse().unwrap()).unwrap(),
            record_one
        );
        assert_eq!(
            db.get("http://example.com/".parse().unwrap()).unwrap(),
            record_one
        );    }
}
