// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Signing and random bytes from the Workers runtime's `WebCrypto`.

use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Crypto, CryptoKey};
use worker::{Error, Result};

const RS256: &str = "RSASSA-PKCS1-v1_5";
const ED25519: &str = "Ed25519";

fn crypto() -> Result<Crypto> {
    Ok(Reflect::get(&js_sys::global(), &"crypto".into())?.unchecked_into())
}

#[allow(clippy::needless_pass_by_value)] // map_err passes the value
fn js_error(err: JsValue) -> Error {
    Error::RustError(
        err.as_string()
            .or_else(|| {
                err.dyn_ref::<js_sys::Error>()
                    .map(|err| String::from(err.message()))
            })
            .unwrap_or_else(|| format!("{err:?}")),
    )
}

/// `n` random bytes.
///
/// # Errors
///
/// When the runtime has no `crypto`.
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; n];
    crypto()?
        .get_random_values_with_u8_array(&mut bytes)
        .map_err(js_error)?;
    Ok(bytes)
}

/// Signs `data` with RS256 using a PKCS#8 RSA private key (see
/// [`crate::jwt::pkcs8_der`]).
///
/// # Errors
///
/// When `WebCrypto` refuses the key.
pub async fn sign_rs256(pkcs8: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let algorithm = Object::new();
    Reflect::set(&algorithm, &"name".into(), &RS256.into())?;
    Reflect::set(&algorithm, &"hash".into(), &"SHA-256".into())?;
    sign(pkcs8, &algorithm, RS256, data).await
}

/// Signs `data` with Ed25519 using a PKCS#8 Ed25519 private key (see
/// [`crate::jwt::ed25519_pkcs8_der`]).
///
/// # Errors
///
/// When `WebCrypto` refuses the key.
pub async fn sign_ed25519(pkcs8: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let algorithm = Object::new();
    Reflect::set(&algorithm, &"name".into(), &ED25519.into())?;
    sign(pkcs8, &algorithm, ED25519, data).await
}

async fn sign(pkcs8: &[u8], algorithm: &Object, name: &str, data: &[u8]) -> Result<Vec<u8>> {
    let subtle = crypto()?.subtle();
    let usages = Array::of1(&"sign".into());
    let key: CryptoKey = JsFuture::from(
        subtle
            .import_key_with_object(
                "pkcs8",
                &Uint8Array::from(pkcs8).into(),
                algorithm,
                false,
                &usages,
            )
            .map_err(js_error)?,
    )
    .await
    .map_err(js_error)?
    .unchecked_into();
    let signature: ArrayBuffer = JsFuture::from(
        subtle
            .sign_with_str_and_u8_array(name, &key, data)
            .map_err(js_error)?,
    )
    .await
    .map_err(js_error)?
    .unchecked_into();
    Ok(Uint8Array::new(&signature).to_vec())
}
