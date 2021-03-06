//! Functions for creating transactions.

use ff::{PrimeField, PrimeFieldRepr};
use pairing::bls12_381::Bls12;
use rusqlite::{types::ToSql, Connection, NO_PARAMS};
use std::path::Path;
use zcash_client_backend::encoding::encode_extended_full_viewing_key;
use zcash_primitives::{
    jubjub::fs::{Fs, FsRepr},
    merkle_tree::IncrementalWitness,
    note_encryption::Memo,
    primitives::{Diversifier, Note},
    prover::TxProver,
    sapling::Node,
    transaction::{
        builder::Builder,
        components::{amount::DEFAULT_FEE, Amount},
    },
    zip32::{ExtendedFullViewingKey, ExtendedSpendingKey},
    JUBJUB,
};

use crate::{
    address::RecipientAddress,
    error::{Error, ErrorKind},
    get_target_and_anchor_heights, HRP_SAPLING_EXTENDED_FULL_VIEWING_KEY,
};

struct SelectedNoteRow {
    diversifier: Diversifier,
    note: Note<Bls12>,
    witness: IncrementalWitness<Node>,
}

/// Creates a transaction paying the specified address from the given account.
///
/// Returns the row index of the newly-created transaction in the `transactions` table
/// within the data database. The caller can read the raw transaction bytes from the `raw`
/// column in order to broadcast the transaction to the network.
///
/// Do not call this multiple times in parallel, or you will generate transactions that
/// double-spend the same notes.
///
/// # Examples
///
/// ```
/// use zcash_client_backend::{
///     constants::{testnet::COIN_TYPE, SAPLING_CONSENSUS_BRANCH_ID},
///     keys::spending_key,
/// };
/// use zcash_client_sqlite::transact::create_to_address;
/// use zcash_primitives::transaction::components::Amount;
/// use zcash_proofs::prover::LocalTxProver;
///
/// let tx_prover = match LocalTxProver::with_default_location() {
///     Some(tx_prover) => tx_prover,
///     None => {
///         panic!("Cannot locate the Zcash parameters. Please run zcash-fetch-params or fetch-params.sh to download the parameters, and then re-run the tests.");
///     }
/// };
///
/// let account = 0;
/// let extsk = spending_key(&[0; 32][..], COIN_TYPE, account);
/// let to = extsk.default_address().unwrap().1.into();
/// match create_to_address(
///     "/path/to/data.db",
///     SAPLING_CONSENSUS_BRANCH_ID,
///     tx_prover,
///     (account, &extsk),
///     &to,
///     Amount::from_u64(1).unwrap(),
///     None,
/// ) {
///     Ok(tx_row) => (),
///     Err(e) => (),
/// }
/// ```
pub fn create_to_address<P: AsRef<Path>>(
    db_data: P,
    consensus_branch_id: u32,
    prover: impl TxProver,
    (account, extsk): (u32, &ExtendedSpendingKey),
    to: &RecipientAddress,
    value: Amount,
    memo: Option<Memo>,
) -> Result<i64, Error> {
    let data = Connection::open(db_data)?;

    // Check that the ExtendedSpendingKey we have been given corresponds to the
    // ExtendedFullViewingKey for the account we are spending from.
    let extfvk = ExtendedFullViewingKey::from(extsk);
    if !data
        .prepare("SELECT * FROM accounts WHERE account = ? AND extfvk = ?")?
        .exists(&[
            account.to_sql()?,
            encode_extended_full_viewing_key(HRP_SAPLING_EXTENDED_FULL_VIEWING_KEY, &extfvk)
                .to_sql()?,
        ])?
    {
        return Err(Error(ErrorKind::InvalidExtSK(account)));
    }
    let ovk = extfvk.fvk.ovk;

    // Target the next block, assuming we are up-to-date.
    let (height, anchor_height) = {
        let (target_height, anchor_height) = get_target_and_anchor_heights(&data)?;
        (target_height, i64::from(anchor_height))
    };

    // The goal of this SQL statement is to select the oldest notes until the required
    // value has been reached, and then fetch the witnesses at the desired height for the
    // selected notes. This is achieved in several steps:
    //
    // 1) Use a window function to create a view of all notes, ordered from oldest to
    //    newest, with an additional column containing a running sum:
    //    - Unspent notes accumulate the values of all unspent notes in that note's
    //      account, up to itself.
    //    - Spent notes accumulate the values of all notes in the transaction they were
    //      spent in, up to itself.
    //
    // 2) Select all unspent notes in the desired account, along with their running sum.
    //
    // 3) Select all notes for which the running sum was less than the required value, as
    //    well as a single note for which the sum was greater than or equal to the
    //    required value, bringing the sum of all selected notes across the threshold.
    //
    // 4) Match the selected notes against the witnesses at the desired height.
    let target_value = i64::from(value + DEFAULT_FEE);
    let mut stmt_select_notes = data.prepare(
        "WITH selected AS (
            WITH eligible AS (
                SELECT id_note, diversifier, value, rcm,
                    SUM(value) OVER
                        (PARTITION BY account, spent ORDER BY id_note) AS so_far
                FROM received_notes
                INNER JOIN transactions ON transactions.id_tx = received_notes.tx
                WHERE account = ? AND spent IS NULL AND transactions.block <= ?
            )
            SELECT * FROM eligible WHERE so_far < ?
            UNION
            SELECT * FROM (SELECT * FROM eligible WHERE so_far >= ? LIMIT 1)
        ), witnesses AS (
            SELECT note, witness FROM sapling_witnesses
            WHERE block = ?
        )
        SELECT selected.diversifier, selected.value, selected.rcm, witnesses.witness
        FROM selected
        INNER JOIN witnesses ON selected.id_note = witnesses.note",
    )?;

    // Select notes
    let notes = stmt_select_notes.query_and_then::<_, Error, _, _>(
        &[
            i64::from(account),
            anchor_height,
            target_value,
            target_value,
            anchor_height,
        ],
        |row| {
            let diversifier = {
                let d: Vec<_> = row.get(0)?;
                if d.len() != 11 {
                    return Err(Error(ErrorKind::CorruptedData(
                        "Invalid diversifier length",
                    )));
                }
                let mut tmp = [0; 11];
                tmp.copy_from_slice(&d);
                Diversifier(tmp)
            };

            let note_value: i64 = row.get(1)?;

            let rcm = {
                let d: Vec<_> = row.get(2)?;
                let mut tmp = FsRepr::default();
                tmp.read_le(&d[..])?;
                Fs::from_repr(tmp).map_err(|_| Error(ErrorKind::InvalidNote))?
            };

            let from = extfvk
                .fvk
                .vk
                .into_payment_address(diversifier, &JUBJUB)
                .unwrap();
            let note = from.create_note(note_value as u64, rcm, &JUBJUB).unwrap();

            let witness = {
                let d: Vec<_> = row.get(3)?;
                IncrementalWitness::read(&d[..])?
            };

            Ok(SelectedNoteRow {
                diversifier,
                note,
                witness,
            })
        },
    )?;
    let notes: Vec<SelectedNoteRow> = notes.collect::<Result<_, _>>()?;

    // Confirm we were able to select sufficient value
    let selected_value = notes
        .iter()
        .fold(0, |acc, selected| acc + selected.note.value);
    if selected_value < target_value as u64 {
        return Err(Error(ErrorKind::InsufficientBalance(
            selected_value,
            target_value as u64,
        )));
    }

    // Create the transaction
    let mut builder = Builder::new(height);
    for selected in notes {
        builder.add_sapling_spend(
            extsk.clone(),
            selected.diversifier,
            selected.note,
            selected.witness,
        )?;
    }
    match to {
        RecipientAddress::Shielded(to) => {
            builder.add_sapling_output(ovk, to.clone(), value, memo.clone())
        }
        RecipientAddress::Transparent(to) => builder.add_transparent_output(&to, value),
    }?;
    let (tx, tx_metadata) = builder.build(consensus_branch_id, prover)?;
    // We only called add_sapling_output() once.
    let output_index = match tx_metadata.output_index(0) {
        Some(idx) => idx as i64,
        None => panic!("Output 0 should exist in the transaction"),
    };
    let created = time::get_time();

    // Update the database atomically, to ensure the result is internally consistent.
    data.execute("BEGIN IMMEDIATE", NO_PARAMS)?;

    // Save the transaction in the database.
    let mut raw_tx = vec![];
    tx.write(&mut raw_tx)?;
    let mut stmt_insert_tx = data.prepare(
        "INSERT INTO transactions (txid, created, expiry_height, raw)
        VALUES (?, ?, ?, ?)",
    )?;
    stmt_insert_tx.execute(&[
        tx.txid().0.to_sql()?,
        created.to_sql()?,
        tx.expiry_height.to_sql()?,
        raw_tx.to_sql()?,
    ])?;
    let id_tx = data.last_insert_rowid();

    // Mark notes as spent.
    //
    // This locks the notes so they aren't selected again by a subsequent call to
    // create_to_address() before this transaction has been mined (at which point the notes
    // get re-marked as spent).
    //
    // Assumes that create_to_address() will never be called in parallel, which is a
    // reasonable assumption for a light client such as a mobile phone.
    let mut stmt_mark_spent_note =
        data.prepare("UPDATE received_notes SET spent = ? WHERE nf = ?")?;
    for spend in &tx.shielded_spends {
        stmt_mark_spent_note.execute(&[id_tx.to_sql()?, spend.nullifier.to_sql()?])?;
    }

    // Save the sent note in the database.
    // TODO: Decide how to save transparent output information.
    let to_str = to.to_string();
    if let Some(memo) = memo {
        let mut stmt_insert_sent_note = data.prepare(
            "INSERT INTO sent_notes (tx, output_index, from_account, address, value, memo)
            VALUES (?, ?, ?, ?, ?, ?)",
        )?;
        stmt_insert_sent_note.execute(&[
            id_tx.to_sql()?,
            output_index.to_sql()?,
            account.to_sql()?,
            to_str.to_sql()?,
            i64::from(value).to_sql()?,
            memo.as_bytes().to_sql()?,
        ])?;
    } else {
        let mut stmt_insert_sent_note = data.prepare(
            "INSERT INTO sent_notes (tx, output_index, from_account, address, value)
            VALUES (?, ?, ?, ?, ?)",
        )?;
        stmt_insert_sent_note.execute(&[
            id_tx.to_sql()?,
            output_index.to_sql()?,
            account.to_sql()?,
            to_str.to_sql()?,
            i64::from(value).to_sql()?,
        ])?;
    }

    data.execute("COMMIT", NO_PARAMS)?;

    // Return the row number of the transaction, so the caller can fetch it for sending.
    Ok(id_tx)
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;
    use zcash_primitives::{
        block::BlockHash,
        prover::TxProver,
        transaction::components::Amount,
        zip32::{ExtendedFullViewingKey, ExtendedSpendingKey},
    };
    use zcash_proofs::prover::LocalTxProver;

    use super::create_to_address;
    use crate::{
        init::{init_accounts_table, init_blocks_table, init_cache_database, init_data_database},
        query::{get_balance, get_verified_balance},
        scan::scan_cached_blocks,
        tests::{fake_compact_block, insert_into_cache},
        SAPLING_ACTIVATION_HEIGHT,
    };

    fn test_prover() -> impl TxProver {
        match LocalTxProver::with_default_location() {
            Some(tx_prover) => tx_prover,
            None => {
                panic!("Cannot locate the Zcash parameters. Please run zcash-fetch-params or fetch-params.sh to download the parameters, and then re-run the tests.");
            }
        }
    }

    #[test]
    fn create_to_address_fails_on_incorrect_extsk() {
        let data_file = NamedTempFile::new().unwrap();
        let db_data = data_file.path();
        init_data_database(&db_data).unwrap();

        // Add two accounts to the wallet
        let extsk0 = ExtendedSpendingKey::master(&[]);
        let extsk1 = ExtendedSpendingKey::master(&[0]);
        let extfvks = [
            ExtendedFullViewingKey::from(&extsk0),
            ExtendedFullViewingKey::from(&extsk1),
        ];
        init_accounts_table(&db_data, &extfvks).unwrap();
        let to = extsk0.default_address().unwrap().1.into();

        // Invalid extsk for the given account should cause an error
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk1),
            &to,
            Amount::from_u64(1).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(e.to_string(), "Incorrect ExtendedSpendingKey for account 0"),
        }
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (1, &extsk0),
            &to,
            Amount::from_u64(1).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(e.to_string(), "Incorrect ExtendedSpendingKey for account 1"),
        }
    }

    #[test]
    fn create_to_address_fails_with_no_blocks() {
        let data_file = NamedTempFile::new().unwrap();
        let db_data = data_file.path();
        init_data_database(&db_data).unwrap();

        // Add an account to the wallet
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvks = [ExtendedFullViewingKey::from(&extsk)];
        init_accounts_table(&db_data, &extfvks).unwrap();
        let to = extsk.default_address().unwrap().1.into();

        // We cannot do anything if we aren't synchronised
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(1).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(e.to_string(), "Must scan blocks first"),
        }
    }

    #[test]
    fn create_to_address_fails_on_insufficient_balance() {
        let data_file = NamedTempFile::new().unwrap();
        let db_data = data_file.path();
        init_data_database(&db_data).unwrap();
        init_blocks_table(&db_data, 1, BlockHash([1; 32]), 1, &[]).unwrap();

        // Add an account to the wallet
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvks = [ExtendedFullViewingKey::from(&extsk)];
        init_accounts_table(&db_data, &extfvks).unwrap();
        let to = extsk.default_address().unwrap().1.into();

        // Account balance should be zero
        assert_eq!(get_balance(db_data, 0).unwrap(), Amount::zero());

        // We cannot spend anything
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(1).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(
                e.to_string(),
                "Insufficient balance (have 0, need 10001 including fee)"
            ),
        }
    }

    #[test]
    fn create_to_address_fails_on_unverified_notes() {
        let cache_file = NamedTempFile::new().unwrap();
        let db_cache = cache_file.path();
        init_cache_database(&db_cache).unwrap();

        let data_file = NamedTempFile::new().unwrap();
        let db_data = data_file.path();
        init_data_database(&db_data).unwrap();

        // Add an account to the wallet
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvk = ExtendedFullViewingKey::from(&extsk);
        init_accounts_table(&db_data, &[extfvk.clone()]).unwrap();

        // Add funds to the wallet in a single note
        let value = Amount::from_u64(50000).unwrap();
        let (cb, _) = fake_compact_block(
            SAPLING_ACTIVATION_HEIGHT,
            BlockHash([0; 32]),
            extfvk.clone(),
            value,
        );
        insert_into_cache(db_cache, &cb);
        scan_cached_blocks(db_cache, db_data).unwrap();

        // Verified balance matches total balance
        assert_eq!(get_balance(db_data, 0).unwrap(), value);
        assert_eq!(get_verified_balance(db_data, 0).unwrap(), value);

        // Add more funds to the wallet in a second note
        let (cb, _) = fake_compact_block(
            SAPLING_ACTIVATION_HEIGHT + 1,
            cb.hash(),
            extfvk.clone(),
            value,
        );
        insert_into_cache(db_cache, &cb);
        scan_cached_blocks(db_cache, db_data).unwrap();

        // Verified balance does not include the second note
        assert_eq!(get_balance(db_data, 0).unwrap(), value + value);
        assert_eq!(get_verified_balance(db_data, 0).unwrap(), value);

        // Spend fails because there are insufficient verified notes
        let extsk2 = ExtendedSpendingKey::master(&[]);
        let to = extsk2.default_address().unwrap().1.into();
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(70000).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(
                e.to_string(),
                "Insufficient balance (have 50000, need 80000 including fee)"
            ),
        }

        // Mine blocks SAPLING_ACTIVATION_HEIGHT + 2 to 9 until just before the second
        // note is verified
        for i in 2..10 {
            let (cb, _) = fake_compact_block(
                SAPLING_ACTIVATION_HEIGHT + i,
                cb.hash(),
                extfvk.clone(),
                value,
            );
            insert_into_cache(db_cache, &cb);
        }
        scan_cached_blocks(db_cache, db_data).unwrap();

        // Second spend still fails
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(70000).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(
                e.to_string(),
                "Insufficient balance (have 50000, need 80000 including fee)"
            ),
        }

        // Mine block 11 so that the second note becomes verified
        let (cb, _) = fake_compact_block(
            SAPLING_ACTIVATION_HEIGHT + 10,
            cb.hash(),
            extfvk.clone(),
            value,
        );
        insert_into_cache(db_cache, &cb);
        scan_cached_blocks(db_cache, db_data).unwrap();

        // Second spend should now succeed
        create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(70000).unwrap(),
            None,
        )
        .unwrap();
    }

    #[test]
    fn create_to_address_fails_on_locked_notes() {
        let cache_file = NamedTempFile::new().unwrap();
        let db_cache = cache_file.path();
        init_cache_database(&db_cache).unwrap();

        let data_file = NamedTempFile::new().unwrap();
        let db_data = data_file.path();
        init_data_database(&db_data).unwrap();

        // Add an account to the wallet
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvk = ExtendedFullViewingKey::from(&extsk);
        init_accounts_table(&db_data, &[extfvk.clone()]).unwrap();

        // Add funds to the wallet in a single note
        let value = Amount::from_u64(50000).unwrap();
        let (cb, _) = fake_compact_block(
            SAPLING_ACTIVATION_HEIGHT,
            BlockHash([0; 32]),
            extfvk.clone(),
            value,
        );
        insert_into_cache(db_cache, &cb);
        scan_cached_blocks(db_cache, db_data).unwrap();
        assert_eq!(get_balance(db_data, 0).unwrap(), value);

        // Send some of the funds to another address
        let extsk2 = ExtendedSpendingKey::master(&[]);
        let to = extsk2.default_address().unwrap().1.into();
        create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(15000).unwrap(),
            None,
        )
        .unwrap();

        // A second spend fails because there are no usable notes
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(2000).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(
                e.to_string(),
                "Insufficient balance (have 0, need 12000 including fee)"
            ),
        }

        // Mine blocks SAPLING_ACTIVATION_HEIGHT + 1 to 21 (that don't send us funds)
        // until just before the first transaction expires
        for i in 1..22 {
            let (cb, _) = fake_compact_block(
                SAPLING_ACTIVATION_HEIGHT + i,
                cb.hash(),
                ExtendedFullViewingKey::from(&ExtendedSpendingKey::master(&[i as u8])),
                value,
            );
            insert_into_cache(db_cache, &cb);
        }
        scan_cached_blocks(db_cache, db_data).unwrap();

        // Second spend still fails
        match create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(2000).unwrap(),
            None,
        ) {
            Ok(_) => panic!("Should have failed"),
            Err(e) => assert_eq!(
                e.to_string(),
                "Insufficient balance (have 0, need 12000 including fee)"
            ),
        }

        // Mine block SAPLING_ACTIVATION_HEIGHT + 22 so that the first transaction expires
        let (cb, _) = fake_compact_block(
            SAPLING_ACTIVATION_HEIGHT + 22,
            cb.hash(),
            ExtendedFullViewingKey::from(&ExtendedSpendingKey::master(&[22])),
            value,
        );
        insert_into_cache(db_cache, &cb);
        scan_cached_blocks(db_cache, db_data).unwrap();

        // Second spend should now succeed
        create_to_address(
            db_data,
            1,
            test_prover(),
            (0, &extsk),
            &to,
            Amount::from_u64(2000).unwrap(),
            None,
        )
        .unwrap();
    }
}
