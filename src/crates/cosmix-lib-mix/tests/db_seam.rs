//! The `DbHandler` seam (`db_query`/`db_exec`) + the `url_decode`/
//! `url_encode` builtins. Mirrors the host-seam pattern of `BusHandler`:
//! a stub handler is injected, the builtins dispatch to it, and the
//! `CapabilityClass::Db` gate denies access when the policy withholds it.

use std::cell::RefCell;
use std::rc::Rc;

use cosmix_mix::evaluator::{DbFuture, Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{CapabilityClass, CategoryAllowList, DbHandler};

/// Records every call and returns canned results so tests can assert
/// both the dispatch (params reached the handler) and the result shape.
#[derive(Default)]
struct StubDb {
    calls: RefCell<Vec<(String, Vec<String>)>>,
}

impl StubDb {
    fn record(&self, sql: &str, params: &[Value]) {
        self.calls.borrow_mut().push((
            sql.to_string(),
            params.iter().map(|v| v.to_mix_string()).collect(),
        ));
    }
}

impl DbHandler for StubDb {
    fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [Value],
    ) -> DbFuture<'a, cosmix_mix::MixResult<Value>> {
        self.record(sql, params);
        Box::pin(async move {
            // Two canned rows as a list of string values (avoids needing
            // IndexMap in the test crate).
            Ok(Value::list(vec![
                Value::String("row-a".into()),
                Value::String("row-b".into()),
            ]))
        })
    }

    fn exec<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [Value],
    ) -> DbFuture<'a, cosmix_mix::MixResult<Value>> {
        self.record(sql, params);
        Box::pin(async move { Ok(Value::Number(42.0)) })
    }
}

/// Parse + run `source` with `configure` applied; return the program's
/// final value (Err mapped to its message).
async fn run_with(source: &str, configure: impl FnOnce(&mut Evaluator)) -> Result<Value, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout), Box::new(stderr));
    configure(&mut eval);
    eval.execute(&stmts).await.map_err(|e| e.to_string())
}

fn allow_db(eval: &mut Evaluator, db: Rc<StubDb>) {
    eval.set_db_handler(db);
    eval.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Db])));
}

#[tokio::test]
async fn db_query_dispatches_to_handler_and_returns_rows() {
    let db = Rc::new(StubDb::default());
    let db2 = db.clone();
    let out = run_with("db_query(\"SELECT * FROM posts\", [])\n", move |e| {
        allow_db(e, db2)
    })
    .await
    .expect("db_query allowed under the Db capability");
    match &out {
        Value::List(rows) => assert_eq!(rows.len(), 2, "stub returns two rows"),
        other => panic!("expected a list of rows, got {other:?}"),
    }
    let calls = db.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "SELECT * FROM posts");
}

#[tokio::test]
async fn db_exec_passes_positional_params() {
    let db = Rc::new(StubDb::default());
    let db2 = db.clone();
    let out = run_with(
        "db_exec(\"INSERT INTO posts (slug, n) VALUES (?1, ?2)\", [\"hello\", 7])\n",
        move |e| allow_db(e, db2),
    )
    .await
    .expect("db_exec allowed");
    assert_eq!(out.to_number(), Some(42.0), "stub exec returns 42");
    let calls = db.calls.borrow();
    assert_eq!(calls.len(), 1);
    // Positional bind params reached the handler in order.
    assert_eq!(calls[0].1, vec!["hello".to_string(), "7".to_string()]);
}

#[tokio::test]
async fn db_denied_without_capability() {
    // Handler present, but the policy does not grant Db → denied before
    // the handler is consulted.
    let db = Rc::new(StubDb::default());
    let db2 = db.clone();
    let err = run_with("db_query(\"SELECT 1\", [])\n", move |e| {
        e.set_db_handler(db2);
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])));
    })
    .await
    .expect_err("Db not in the allow-set must deny db_query");
    assert!(err.contains("capability denied"), "got: {err}");
    assert!(
        db.calls.borrow().is_empty(),
        "handler must NOT be called when denied"
    );
}

#[tokio::test]
async fn db_no_handler_is_clean_error_not_panic() {
    // Db allowed by policy, but no handler installed → catchable error.
    let err = run_with("db_query(\"SELECT 1\", [])\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Db])));
    })
    .await
    .expect_err("no db handler must error");
    assert!(err.contains("database not available"), "got: {err}");
}

#[tokio::test]
async fn db_query_rejects_non_list_params() {
    let db = Rc::new(StubDb::default());
    let db2 = db.clone();
    let err = run_with("db_query(\"SELECT 1\", \"oops\")\n", move |e| {
        allow_db(e, db2)
    })
    .await
    .expect_err("a non-list params arg is a usage error");
    assert!(err.contains("list of bind params"), "got: {err}");
}

#[tokio::test]
async fn url_decode_handles_percent_and_plus() {
    let out = run_with("url_decode(\"a%20b+c%2Fd\")\n", |_| {})
        .await
        .expect("url_decode is a pure builtin");
    assert_eq!(out, Value::String("a b c/d".into()));
}

#[tokio::test]
async fn url_decode_passes_through_bad_escapes() {
    let out = run_with("url_decode(\"100%\")\n", |_| {})
        .await
        .expect("trailing % is passed through");
    assert_eq!(out, Value::String("100%".into()));
}

#[tokio::test]
async fn url_encode_roundtrips() {
    let out = run_with("url_encode(\"a b/c~d\")\n", |_| {})
        .await
        .expect("url_encode is a pure builtin");
    // space → %20, '/' → %2F, '~' unreserved (kept).
    assert_eq!(out, Value::String("a%20b%2Fc~d".into()));
}

#[tokio::test]
async fn html_escape_escapes_the_five_chars() {
    let out = run_with("html_escape(\"<a href='x'>&\\\"q\\\"</a>\")\n", |_| {})
        .await
        .expect("html_escape is a pure builtin");
    assert_eq!(
        out,
        Value::String("&lt;a href=&#x27;x&#x27;&gt;&amp;&quot;q&quot;&lt;/a&gt;".into())
    );
}

#[tokio::test]
async fn html_escape_neutralises_script_xss() {
    let out = run_with("html_escape(\"<script>alert('x')</script>\")\n", |_| {})
        .await
        .expect("html_escape");
    assert_eq!(
        out,
        Value::String("&lt;script&gt;alert(&#x27;x&#x27;)&lt;/script&gt;".into())
    );
}

#[tokio::test]
async fn html_escape_ampersand_not_double_escaped() {
    // Each '&' is escaped exactly once — an already-entity-looking input
    // becomes literal text (safe; we never try to detect prior entities).
    let out = run_with("html_escape(\"a & b &lt;\")\n", |_| {})
        .await
        .expect("html_escape");
    assert_eq!(out, Value::String("a &amp; b &amp;lt;".into()));
}

#[tokio::test]
async fn parse_query_decodes_pairs() {
    let out = run_with(
        "parse_query(\"title=Hello+World&slug=a%2Fb&n=7\")\n",
        |_| {},
    )
    .await
    .expect("parse_query is a pure builtin");
    let Value::Map(m) = &out else {
        panic!("expected a map")
    };
    assert_eq!(m.get("title"), Some(&Value::String("Hello World".into())));
    assert_eq!(m.get("slug"), Some(&Value::String("a/b".into())));
    assert_eq!(m.get("n"), Some(&Value::String("7".into())));
}

#[tokio::test]
async fn parse_query_handles_edge_cases() {
    // leading/trailing/double '&' skipped; bare key + "k=" → "".
    let out = run_with("parse_query(\"&flag&k=&&x=1&\")\n", |_| {})
        .await
        .expect("parse_query");
    let Value::Map(m) = &out else {
        panic!("expected a map")
    };
    assert_eq!(m.len(), 3, "empty pairs are not entries");
    assert_eq!(m.get("flag"), Some(&Value::String(String::new())));
    assert_eq!(m.get("k"), Some(&Value::String(String::new())));
    assert_eq!(m.get("x"), Some(&Value::String("1".into())));
}

#[tokio::test]
async fn parse_query_last_value_wins() {
    let out = run_with("parse_query(\"a=1&a=2&a=3\")\n", |_| {})
        .await
        .expect("parse_query");
    let Value::Map(m) = &out else {
        panic!("expected a map")
    };
    assert_eq!(m.len(), 1);
    assert_eq!(m.get("a"), Some(&Value::String("3".into())));
}

#[tokio::test]
async fn parse_query_empty_is_empty_map() {
    let out = run_with("parse_query(\"\")\n", |_| {})
        .await
        .expect("parse_query");
    let Value::Map(m) = &out else {
        panic!("expected a map")
    };
    assert_eq!(m.len(), 0);
}

#[tokio::test]
async fn parse_form_aliases_parse_query() {
    let out = run_with("parse_form(\"name=a+b&n=7\")\n", |_| {})
        .await
        .expect("parse_form");
    let Value::Map(m) = &out else {
        panic!("expected a map")
    };
    assert_eq!(m.get("name"), Some(&Value::String("a b".into())));
    assert_eq!(m.get("n"), Some(&Value::String("7".into())));
}
