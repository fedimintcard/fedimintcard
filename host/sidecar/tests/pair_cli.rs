//! `card-sidecar till pair`: the POS token (the whole till API) and, with `--scope pay`, the pay
//! token (paying invoices with a card only), each in its own private file of the data directory.

use std::process::Command;

use fedimint_card_sidecar::pos;

fn pair(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_card-sidecar"))
        .arg("--data-dir")
        .arg(dir)
        .args(["till", "pair", "--text", "--url", "https://till.example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn pairing_with_the_pay_scope_makes_a_pay_token_beside_the_pos_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let text = pair(dir.path(), &["--scope", "pay"]);
    let pay = pos::read_pay_token(dir.path())?.expect("a pay token");
    assert_eq!(text, format!("fedimintcard-till-pay:https://till.example.com?t={pay}"));
    // The POS token is made too: a till listens on the LAN only with it.
    let token = pos::read_token(dir.path())?.expect("a POS token");
    assert_ne!(token, pay);
    // Again: the same token. With --new-token: a new pay token, the POS token unchanged.
    assert_eq!(pair(dir.path(), &["--scope", "pay"]), text);
    let new = pair(dir.path(), &["--scope", "pay", "--new-token"]);
    assert_ne!(new, text);
    assert_eq!(pos::read_token(dir.path())?, Some(token.clone()));
    // The default scope pairs a POS phone, with the POS token, as before.
    assert_eq!(pair(dir.path(), &[]), format!("fedimintcard-till:https://till.example.com?t={token}"));
    Ok(())
}
