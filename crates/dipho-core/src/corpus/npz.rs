//! Minimal `prosody.npz` reader: a zip archive of `.npy` entries written by
//! `np.savez` (stored, uncompressed). Only what the contract needs — little-
//! endian float32, C-order, 1-D `f0`/`rms_db` and 2-D `mfcc` — everything
//! else is a typed rejection, never a guess.

use std::io::{Cursor, Read};

use super::CorpusError;
use super::features::{MFCC_DIM, ProsodyData};

fn npz_err(msg: impl Into<String>) -> CorpusError {
    CorpusError::Npz(msg.into())
}

/// Parse the three frame arrays out of `prosody.npz` bytes. `hop` comes from
/// the manifest; frame-count agreement with the manifest is the loader's
/// check, not ours.
pub fn prosody_from_npz(bytes: &[u8], hop: f64) -> Result<ProsodyData, CorpusError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| npz_err(format!("not a zip archive: {e}")))?;

    let f0 = read_array(&mut zip, "f0")?;
    let rms_db = read_array(&mut zip, "rms_db")?;
    let mfcc = read_array(&mut zip, "mfcc")?;

    let (f0, f0_shape) = f0;
    let (rms_db, rms_shape) = rms_db;
    let (mfcc, mfcc_shape) = mfcc;
    if f0_shape.len() != 1 || rms_shape.len() != 1 {
        return Err(npz_err("f0 and rms_db must be 1-D"));
    }
    match mfcc_shape.as_slice() {
        [_, dim] if *dim == MFCC_DIM => {}
        other => {
            return Err(npz_err(format!(
                "mfcc shape {other:?}, expected [n, {MFCC_DIM}]"
            )));
        }
    }
    Ok(ProsodyData {
        hop,
        f0,
        rms_db,
        mfcc,
    })
}

fn read_array(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<(Vec<f32>, Vec<usize>), CorpusError> {
    let mut entry = zip
        .by_name(&format!("{name}.npy"))
        .map_err(|e| npz_err(format!("missing entry {name}.npy: {e}")))?;
    let mut data = Vec::new();
    entry
        .read_to_end(&mut data)
        .map_err(|e| npz_err(format!("reading {name}.npy: {e}")))?;
    parse_npy_f32(&data).map_err(|msg| npz_err(format!("{name}.npy: {msg}")))
}

/// Parse one `.npy` payload: magic + version + header dict + raw data.
fn parse_npy_f32(data: &[u8]) -> Result<(Vec<f32>, Vec<usize>), String> {
    if data.len() < 10 || &data[..6] != b"\x93NUMPY" {
        return Err("bad npy magic".into());
    }
    let (header_len, header_start) = match data[6] {
        1 => (u16::from_le_bytes([data[8], data[9]]) as usize, 10),
        2 | 3 => {
            if data.len() < 12 {
                return Err("truncated npy header".into());
            }
            let len = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
            (len, 12)
        }
        v => return Err(format!("unsupported npy version {v}")),
    };
    let body_start = header_start + header_len;
    let header = std::str::from_utf8(
        data.get(header_start..body_start)
            .ok_or("truncated npy header")?,
    )
    .map_err(|_| "npy header is not utf-8")?;

    let descr = dict_value(header, "descr").ok_or("npy header missing descr")?;
    if descr != "'<f4'" {
        return Err(format!("dtype {descr}, expected '<f4'"));
    }
    let fortran = dict_value(header, "fortran_order").ok_or("npy header missing fortran_order")?;
    if fortran != "False" {
        return Err("fortran_order arrays are not supported".into());
    }
    let shape_src = dict_value(header, "shape").ok_or("npy header missing shape")?;
    let shape: Vec<usize> = shape_src
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .parse::<usize>()
                .map_err(|_| "bad shape".to_string())
        })
        .collect::<Result<_, _>>()?;

    let n: usize = shape.iter().product();
    let body = &data[body_start..];
    if body.len() != n * 4 {
        return Err(format!(
            "data is {} bytes, shape needs {}",
            body.len(),
            n * 4
        ));
    }
    let values = body
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok((values, shape))
}

/// Value of `'key': <value>` in the npy header dict literal, up to the next
/// top-level comma. Values are flat (string, bool, or tuple) — nested
/// parens only occur in `shape`.
fn dict_value<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("'{key}':");
    let rest = &header[header.find(&pat)? + pat.len()..];
    let rest = rest.trim_start();
    let end = if rest.starts_with('(') {
        rest.find(')')? + 1
    } else {
        rest.find([',', '}'])?
    };
    Some(rest[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-assembled npy v1 payload: little-endian f32, C order.
    fn npy(shape: &str, values: &[f32]) -> Vec<u8> {
        let header = format!("{{'descr': '<f4', 'fortran_order': False, 'shape': {shape}, }}");
        let mut padded = header.into_bytes();
        while (10 + padded.len()) % 64 != 0 {
            padded.push(b' ');
        }
        let mut out = b"\x93NUMPY\x01\x00".to_vec();
        out.extend((padded.len() as u16).to_le_bytes());
        out.extend(&padded);
        for v in values {
            out.extend(v.to_le_bytes());
        }
        out
    }

    fn npz(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zip.start_file(format!("{name}.npy"), opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn valid_npz() -> Vec<u8> {
        npz(&[
            ("f0", npy("(3,)", &[0.0, 220.0, 230.0])),
            ("rms_db", npy("(3,)", &[-30.0, -20.0, -25.0])),
            ("mfcc", npy("(3, 13)", &[0.5; 39])),
        ])
    }

    #[test]
    fn parses_a_valid_npz() {
        let p = prosody_from_npz(&valid_npz(), 0.01).unwrap();
        assert_eq!(p.hop, 0.01);
        assert_eq!(p.f0, vec![0.0, 220.0, 230.0]);
        assert_eq!(p.rms_db.len(), 3);
        assert_eq!(p.mfcc.len(), 39);
        assert_eq!(p.n_frames(), 3);
    }

    #[test]
    fn ignores_extra_entries() {
        let mut entries = vec![
            ("f0", npy("(1,)", &[100.0])),
            ("rms_db", npy("(1,)", &[-10.0])),
            ("mfcc", npy("(1, 13)", &[0.0; 13])),
        ];
        entries.push(("input_fingerprint", npy("(1,)", &[0.0])));
        assert!(prosody_from_npz(&npz(&entries), 0.01).is_ok());
    }

    #[test]
    fn rejects_missing_entry() {
        let bytes = npz(&[("f0", npy("(1,)", &[100.0]))]);
        assert!(matches!(
            prosody_from_npz(&bytes, 0.01),
            Err(CorpusError::Npz(msg)) if msg.contains("rms_db")
        ));
    }

    #[test]
    fn rejects_wrong_mfcc_width() {
        let bytes = npz(&[
            ("f0", npy("(1,)", &[100.0])),
            ("rms_db", npy("(1,)", &[-10.0])),
            ("mfcc", npy("(1, 12)", &[0.0; 12])),
        ]);
        assert!(matches!(
            prosody_from_npz(&bytes, 0.01),
            Err(CorpusError::Npz(_))
        ));
    }

    #[test]
    fn rejects_wrong_dtype() {
        let header = "{'descr': '<f8', 'fortran_order': False, 'shape': (1,), }";
        let mut bad = b"\x93NUMPY\x01\x00".to_vec();
        bad.extend((header.len() as u16).to_le_bytes());
        bad.extend(header.as_bytes());
        bad.extend(1.0f64.to_le_bytes());
        let bytes = npz(&[
            ("f0", bad),
            ("rms_db", npy("(1,)", &[0.0])),
            ("mfcc", npy("(1, 13)", &[0.0; 13])),
        ]);
        assert!(matches!(
            prosody_from_npz(&bytes, 0.01),
            Err(CorpusError::Npz(_))
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            prosody_from_npz(b"not a zip", 0.01),
            Err(CorpusError::Npz(_))
        ));
    }
}
