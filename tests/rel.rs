//! Tests de integración de la capa relacional (hito M2) a través de `Store`:
//! esquemas, rowids, tipos en los límites y persistencia real en archivo.

use std::path::PathBuf;

use arkeion::catalog::{ColType, ColumnSpec, TableSpec};
use arkeion::record::Value;
use arkeion::tx::Store;

fn db(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("t.arkeion")
}

fn col(name: &str, t: ColType) -> ColumnSpec {
    ColumnSpec {
        name: name.into(),
        col_type: t,
        not_null: false,
        primary_key: false,
        default: None,
    }
}

fn pedidos_spec() -> TableSpec {
    TableSpec {
        name: "pedidos".into(),
        columns: vec![
            ColumnSpec {
                primary_key: true,
                ..col("id", ColType::Integer)
            },
            col("cliente", ColType::Text),
            col("importe", ColType::Real),
            col("urgente", ColType::Boolean),
            col("firma", ColType::Blob),
        ],
    }
}

#[test]
fn relational_roundtrip_with_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);

    let store = Store::create(&path).unwrap();
    let mut tx = store.begin().unwrap();
    let pedidos = tx.create_table(&pedidos_spec()).unwrap();

    let firma: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
    let id = tx
        .insert_row(
            &pedidos,
            &[
                Value::Null,
                Value::Text("Åke & Söner 🐢".into()),
                Value::Real(1234.56),
                Value::Bool(true),
                Value::Blob(firma.clone()),
            ],
        )
        .unwrap();
    assert_eq!(id, 1);
    // La tx ve su propio DDL y sus propias filas.
    assert!(tx.table("pedidos").unwrap().is_some());
    assert_eq!(
        tx.get_row(&pedidos, id).unwrap().unwrap()[1],
        Value::Text("Åke & Söner 🐢".into())
    );
    tx.commit().unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    let snap = store.snapshot();
    let pedidos = snap.table("pedidos").unwrap().expect("el esquema persiste");
    assert_eq!(pedidos.rowid_alias, Some(0));
    let row = snap.get_row(&pedidos, 1).unwrap().unwrap();
    assert_eq!(row[0], Value::Integer(1));
    assert_eq!(row[2], Value::Real(1234.56));
    assert_eq!(row[3], Value::Bool(true));
    assert_eq!(
        row[4],
        Value::Blob(firma),
        "blob > 1 página (overflow) intacto"
    );
}

#[test]
fn scan_follows_rowid_order_including_negatives() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create(&db(&dir)).unwrap();
    let mut tx = store.begin().unwrap();
    let t = tx.create_table(&pedidos_spec()).unwrap();

    for id in [42i64, -7, 13, i64::MIN, 0, i64::MAX, -1] {
        tx.insert_row(&t, &[Value::Integer(id), Value::Text(format!("p{id}"))])
            .unwrap();
    }
    tx.commit().unwrap();

    let snap = store.snapshot();
    let t = snap.table("pedidos").unwrap().unwrap();
    let rows: Vec<(i64, Vec<Value>)> = snap.scan_table(&t).unwrap().map(|r| r.unwrap()).collect();
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![i64::MIN, -7, -1, 0, 13, 42, i64::MAX]);
    for (id, row) in &rows {
        assert_eq!(row[0], Value::Integer(*id));
        assert_eq!(row[1], Value::Text(format!("p{id}")));
    }
}

#[test]
fn thousand_rows_two_tables_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let store = Store::create(&path).unwrap();

    let mut tx = store.begin().unwrap();
    let pedidos = tx.create_table(&pedidos_spec()).unwrap();
    let clientes = tx
        .create_table(&TableSpec {
            name: "clientes".into(),
            columns: vec![col("nombre", ColType::Text), col("saldo", ColType::Real)],
        })
        .unwrap();

    for i in 0..1000i64 {
        tx.insert_row(
            &pedidos,
            &[
                Value::Null,
                Value::Text(format!("cliente-{}", i % 50)),
                Value::Real(i as f64),
            ],
        )
        .unwrap();
        if i % 4 == 0 {
            tx.insert_row(
                &clientes,
                &[Value::Text(format!("c{i}")), Value::Integer(i)],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    let snap = store.snapshot();
    let pedidos = snap.table("pedidos").unwrap().unwrap();
    let clientes = snap.table("clientes").unwrap().unwrap();

    let rows: Vec<_> = snap
        .scan_table(&pedidos)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.len(), 1000);
    assert!(rows.windows(2).all(|w| w[0].0 < w[1].0), "orden de rowid");
    assert_eq!(rows[999].1[2], Value::Real(999.0));

    let crows: Vec<_> = snap
        .scan_table(&clientes)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(crows.len(), 250);
    // Promoción INTEGER→REAL aplicada al insertar en columna REAL.
    assert_eq!(crows[0].1[1], Value::Real(0.0));
}

#[test]
fn deletes_and_drop_table_across_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = db(&dir);
    let store = Store::create(&path).unwrap();

    let mut tx = store.begin().unwrap();
    let t = tx.create_table(&pedidos_spec()).unwrap();
    for i in 1..=20i64 {
        tx.insert_row(&t, &[Value::Integer(i)]).unwrap();
    }
    tx.commit().unwrap();

    let mut tx = store.begin().unwrap();
    for i in (2..=20i64).step_by(2) {
        assert!(tx.delete_row(&t, i).unwrap());
    }
    assert!(!tx.delete_row(&t, 999).unwrap());
    tx.commit().unwrap();

    let snap = store.snapshot();
    let ids: Vec<i64> = snap.scan_table(&t).unwrap().map(|r| r.unwrap().0).collect();
    assert_eq!(ids, (1..=19).step_by(2).collect::<Vec<i64>>());

    // Drop en otra tx; reabrir confirma que tabla y filas desaparecieron.
    let mut tx = store.begin().unwrap();
    assert!(tx.drop_table("pedidos").unwrap());
    tx.commit().unwrap();
    drop(snap);
    drop(store);

    let store = Store::open(&path).unwrap();
    assert!(store.snapshot().table("pedidos").unwrap().is_none());
}

#[test]
fn snapshot_isolation_covers_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create(&db(&dir)).unwrap();

    let before = store.snapshot();
    let mut tx = store.begin().unwrap();
    let t = tx.create_table(&pedidos_spec()).unwrap();
    tx.insert_row(&t, &[Value::Integer(1)]).unwrap();
    tx.commit().unwrap();

    assert!(
        before.table("pedidos").unwrap().is_none(),
        "el snapshot viejo no ve el DDL"
    );
    assert!(store.snapshot().table("pedidos").unwrap().is_some());
}

#[test]
fn constraint_errors_do_not_poison_the_tx() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create(&db(&dir)).unwrap();
    let mut tx = store.begin().unwrap();
    let t = tx.create_table(&pedidos_spec()).unwrap();

    tx.insert_row(&t, &[Value::Integer(7)]).unwrap();
    // Duplicado y tipo incompatible fallan…
    assert!(tx.insert_row(&t, &[Value::Integer(7)]).is_err());
    assert!(
        tx.insert_row(
            &t,
            &[
                Value::Null,
                Value::Real(1.0),
                Value::Null,
                Value::Text("x".into())
            ]
        )
        .is_err()
    );
    // …pero la tx sigue siendo utilizable y consistente.
    let id = tx.insert_row(&t, &[Value::Null]).unwrap();
    assert_eq!(id, 8);
    tx.commit().unwrap();

    let snap = store.snapshot();
    let t = snap.table("pedidos").unwrap().unwrap();
    let ids: Vec<i64> = snap.scan_table(&t).unwrap().map(|r| r.unwrap().0).collect();
    assert_eq!(ids, vec![7, 8]);
}
