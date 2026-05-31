//! Faithful floating-point → text rendering, ported from SQLite 3.53.1's `sqlite3FpDecode`
//! (`util.c`) and the `%g`/`%f` rendering path in `printf.c`.
//!
//! SQLite does NOT use C's `printf("%g")` for rendering REAL values; it has its own decimal
//! decoder that produces a round-trippable (but not always minimal) representation, with a
//! specific reduction step at 17 significant digits. Reproducing it exactly is required for
//! byte-compatible output: `sqlite3_column_text` of a REAL uses `%!.17g`, which is what
//! [`fp_to_text`] implements. The result is validated against the `sqlite3` binary by a
//! differential fuzz test.
//!
//! All arithmetic uses `wrapping_*` where the C source relies on defined unsigned wraparound,
//! so this module is correct under the crate's `overflow-checks = true` dev profile.

// ---- 128/160-bit multiply helpers (util.c `sqlite3Multiply128`/`sqlite3Multiply160`) ----

/// `(a*b) -> (high 64 bits, low 64 bits)`.
fn multiply128(a: u64, b: u64) -> (u64, u64) {
    let r = (a as u128) * (b as u128);
    ((r >> 64) as u64, r as u64)
}

/// `A = (a<<32)+aLo`, `B = b`; return the high 64 bits of `A*B` and write the middle 32 bits
/// into the returned `.1`. The low 64 bits are discarded (mirrors `sqlite3Multiply160`).
fn multiply160(a: u64, a_lo: u32, b: u64) -> (u64, u32) {
    let mut r = (a as u128) * (b as u128);
    r += ((a_lo as u128) * (b as u128)) >> 32;
    let mid = ((r >> 32) & 0xffff_ffff) as u32;
    ((r >> 64) as u64, mid)
}

// ---- base-10 ⇄ base-2 exponent estimates (util.c) ----

fn pwr10to2(p: i32) -> i32 {
    ((p as i64 * 108853) >> 15) as i32
}
fn pwr2to10(p: i32) -> i32 {
    ((p as i64 * 78913) >> 18) as i32
}

const POWERSOF10_FIRST: i32 = -348;
const POWERSOF10_LAST: i32 = 347;

/// `powerOfTen(p)`: return a 96-bit (`hi:u64`, `lo:u32`) approximation of `10^p` scaled into
/// the top bits, used by the decimal decode. Tables copied verbatim from `util.c`.
fn power_of_ten(p: i32) -> (u64, u32) {
    const A_BASE: [u64; 27] = [
        0x8000000000000000,
        0xa000000000000000,
        0xc800000000000000,
        0xfa00000000000000,
        0x9c40000000000000,
        0xc350000000000000,
        0xf424000000000000,
        0x9896800000000000,
        0xbebc200000000000,
        0xee6b280000000000,
        0x9502f90000000000,
        0xba43b74000000000,
        0xe8d4a51000000000,
        0x9184e72a00000000,
        0xb5e620f480000000,
        0xe35fa931a0000000,
        0x8e1bc9bf04000000,
        0xb1a2bc2ec5000000,
        0xde0b6b3a76400000,
        0x8ac7230489e80000,
        0xad78ebc5ac620000,
        0xd8d726b7177a8000,
        0x878678326eac9000,
        0xa968163f0a57b400,
        0xd3c21bcecceda100,
        0x84595161401484a0,
        0xa56fa5b99019a5c8,
    ];
    const A_SCALE: [u64; 26] = [
        0x8049a4ac0c5811ae,
        0xcf42894a5dce35ea,
        0xa76c582338ed2621,
        0x873e4f75e2224e68,
        0xda7f5bf590966848,
        0xb080392cc4349dec,
        0x8e938662882af53e,
        0xe65829b3046b0afa,
        0xba121a4650e4ddeb,
        0x964e858c91ba2655,
        0xf2d56790ab41c2a2,
        0xc428d05aa4751e4c,
        0x9e74d1b791e07e48,
        0xcccccccccccccccc,
        0xcecb8f27f4200f3a,
        0xa70c3c40a64e6c51,
        0x86f0ac99b4e8dafd,
        0xda01ee641a708de9,
        0xb01ae745b101e9e4,
        0x8e41ade9fbebc27d,
        0xe5d3ef282a242e81,
        0xb9a74a0637ce2ee1,
        0x95f83d0a1fb69cd9,
        0xf24a01a73cf2dccf,
        0xc3b8358109e84f07,
        0x9e19db92b4e31ba9,
    ];
    const A_SCALE_LO: [u32; 26] = [
        0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b,
        0x0cb4a5a3, 0x92f34d62, 0x3a6a07f9, 0xfae27299, 0xaa97e14c, 0x775ea265, 0xcccccccc,
        0x00000000, 0x999090b6, 0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9,
        0x6d953e2c, 0x4abdaf10, 0xbc633b39, 0x0a862f81, 0x6c07a2c2,
    ];

    debug_assert!((POWERSOF10_FIRST..=POWERSOF10_LAST).contains(&p));
    let (g, n);
    if p < 0 {
        if p == -1 {
            return (A_SCALE[13], A_SCALE_LO[13]);
        }
        let mut gg = p / 27;
        let mut nn = p % 27;
        if nn != 0 {
            gg -= 1;
            nn += 27;
        }
        g = gg;
        n = nn;
    } else if p < 27 {
        return (A_BASE[p as usize], 0);
    } else {
        g = p / 27;
        n = p % 27;
    }
    let s = A_SCALE[(g + 13) as usize];
    if n == 0 {
        return (s, A_SCALE_LO[(g + 13) as usize]);
    }
    let (mut x, mut lo) = multiply160(s, A_SCALE_LO[(g + 13) as usize], A_BASE[n as usize]);
    if (x & (1u64 << 63)) == 0 {
        x = (x << 1) | ((lo >> 31) & 1) as u64;
        lo = (lo << 1) | 1;
    }
    (x, lo)
}

/// `sqlite3Fp2Convert10`: given `r == m*2^e` (m left-justified), produce `(d, p)` with
/// `r ≈ d*10^p` and `d` holding `n` significant digits.
fn fp2convert10(m: u64, e: i32, n: i32) -> (u64, i32) {
    debug_assert!((1..=18).contains(&n));
    let p = n - 1 - pwr2to10(e + 63);
    let (poweroften_hi, _lo) = power_of_ten(p);
    let (h, _d1) = multiply128(m, poweroften_hi);
    let d = if n == 18 {
        let sh = -(e + pwr10to2(p) + 2);
        let h = h >> sh;
        (h.wrapping_add((h << 1) & 2)) >> 1
    } else {
        let sh = -(e + pwr10to2(p) + 1);
        h >> sh
    };
    (d, -p)
}

/// `sqlite3Fp10Convert2`: return the IEEE-754 double nearest to `d*10^p` (Ross Cox's algorithm).
/// Used to check whether a reduced-precision decimal round-trips to the original value.
fn fp10convert2(d: u64, p: i32) -> f64 {
    if p < POWERSOF10_FIRST {
        return 0.0;
    }
    if p > POWERSOF10_LAST {
        return f64::INFINITY;
    }
    let b = 64 - d.leading_zeros() as i32;
    let lp = pwr10to2(p);
    let mut e = 53 - b - lp;
    if e > 1074 {
        if e >= 1130 {
            return 0.0;
        }
        e = 1074;
    }
    let s = -(e - (64 - b) + lp + 3);
    let (mut pwr10h, pwr10l) = power_of_ten(p);
    let pwr10l = if pwr10l != 0 {
        pwr10h = pwr10h.wrapping_add(1);
        !pwr10l
    } else {
        pwr10l
    };
    let x = d << (64 - b);
    let (mut hi, lo) = multiply128(x, pwr10h);
    let mid1 = (lo >> 32) as u32;
    let mut sticky: u64 = 1;
    if (hi & ((1u64 << s).wrapping_sub(1))) == 0 {
        let (mid2_hi, _mid2_lo) = multiply128(x, (pwr10l as u64) << 32);
        let mid2 = (mid2_hi >> 32) as u32;
        sticky = if mid1.wrapping_sub(mid2) > 1 { 1 } else { 0 };
        hi -= if mid1 < mid2 { 1 } else { 0 };
    }
    let mut u = (hi >> s) | sticky;
    let adj = if u >= (1u64 << 55).wrapping_sub(2) {
        1
    } else {
        0
    };
    if adj == 1 {
        u = (u >> adj) | (u & 1);
        e -= adj;
    }
    let mut m = (u.wrapping_add(1).wrapping_add((u >> 2) & 1)) >> 2;
    if e <= -972 {
        return f64::INFINITY;
    }
    if (m & (1u64 << 52)) != 0 {
        m = (m & !(1u64 << 52)) | (((1075 - e) as u64) << 52);
    }
    f64::from_bits(m)
}

/// The decoded decimal form of a double (mirrors `struct FpDecode`).
struct FpDecode {
    /// `b'+'` or `b'-'`.
    sign: u8,
    /// Number of significant digits in `digits`.
    n: usize,
    /// Position of the decimal point: the value is `0.<digits> * 10^i_dp`, i.e. there are
    /// `i_dp` digits before the decimal point.
    i_dp: i32,
    /// The significant digit characters (`b'0'..=b'9'`), no leading/trailing zeros (except a
    /// lone `"0"` for zero).
    digits: Vec<u8>,
    /// 0 = normal, 1 = Infinity, 2 = NaN.
    is_special: u8,
}

/// Port of `sqlite3FpDecode(p, r, iRound, mxRound)`.
fn fp_decode(r: f64, i_round_in: i32, mx_round: i32) -> FpDecode {
    debug_assert!(mx_round > 0);
    let mut r = r;
    let sign;
    if r < 0.0 {
        sign = b'-';
        r = -r;
    } else if r == 0.0 {
        return FpDecode {
            sign: b'+',
            n: 1,
            i_dp: 1,
            digits: vec![b'0'],
            is_special: 0,
        };
    } else {
        sign = b'+';
    }

    let bits = r.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i32;
    if e == 0x7ff {
        return FpDecode {
            sign,
            n: 0,
            i_dp: 0,
            digits: Vec::new(),
            is_special: 1 + (bits != 0x7ff0000000000000) as u8,
        };
    }
    let mut v = bits & 0x000fffffffffffff;
    if e == 0 {
        let nn = v.leading_zeros() as i32;
        v <<= nn;
        e = -1074 - nn;
    } else {
        v = (v << 11) | (1u64 << 63);
        e -= 1086;
    }

    let conv_n = if i_round_in <= 0 || i_round_in >= 18 {
        18
    } else {
        i_round_in + 1
    };
    let (mut v, exp) = fp2convert10(v, e, conv_n);

    // Extract decimal digits of `v` right-to-left into a 21-byte buffer.
    let mut zbuf = [0u8; 21];
    let mut i = 20usize;
    while v >= 10 {
        let two = (v % 100) as usize;
        zbuf[i - 1] = b'0' + (two % 10) as u8;
        zbuf[i - 2] = b'0' + (two / 10) as u8;
        i -= 2;
        v /= 100;
    }
    if v != 0 {
        i -= 1;
        zbuf[i] = b'0' + v as u8;
    }
    let mut n = (20 - i) as i32;
    let mut i_dp = n + exp;

    let mut i_round = i_round_in;
    if i_round <= 0 {
        i_round = i_dp - i_round;
        if i_round == 0 && zbuf[i] >= b'5' {
            i_round = 1;
            i -= 1;
            zbuf[i] = b'0';
            n += 1;
            i_dp += 1;
        }
    }

    // `z` is the slice starting at `i`; we track its start index so the reduction can step it
    // back (`z--`).
    let mut z = i;
    if i_round > 0 && (i_round < n || n > mx_round) {
        if i_round > mx_round {
            i_round = mx_round;
        }
        if i_round == 17 {
            // Reduce the precision below 17 when a shorter decimal round-trips to `r`.
            let zr = |k: usize| zbuf[z + k];
            if zr(15) == b'9' && zr(14) == b'9' {
                let mut jj = 14usize;
                while jj > 0 && zbuf[z + jj - 1] == b'9' {
                    jj -= 1;
                }
                let v2: u64 = if jj == 0 {
                    1
                } else {
                    let mut acc = (zbuf[z] - b'0') as u64;
                    for kk in 1..jj {
                        acc = acc * 10 + (zbuf[z + kk] - b'0') as u64;
                    }
                    acc + 1
                };
                if r == fp10convert2(v2, exp + n - jj as i32) {
                    i_round = jj as i32 + 1;
                }
            } else if i_dp >= n || (zr(15) == b'0' && zr(14) == b'0' && zr(13) == b'0') {
                let mut jj = 13usize;
                while zbuf[z + jj - 1] == b'0' {
                    jj -= 1;
                }
                let mut acc = (zbuf[z] - b'0') as u64;
                for kk in 1..jj {
                    acc = acc * 10 + (zbuf[z + kk] - b'0') as u64;
                }
                if r == fp10convert2(acc, exp + n - jj as i32) {
                    i_round = jj as i32 + 1;
                }
            }
        }
        n = i_round;
        if zbuf[z + i_round as usize] >= b'5' {
            // Round the kept digits up, carrying as needed.
            let mut j = i_round - 1;
            loop {
                zbuf[z + j as usize] += 1;
                if zbuf[z + j as usize] <= b'9' {
                    break;
                }
                zbuf[z + j as usize] = b'0';
                if j == 0 {
                    z -= 1;
                    zbuf[z] = b'1';
                    n += 1;
                    i_dp += 1;
                    break;
                } else {
                    j -= 1;
                }
            }
        }
    }

    // Strip trailing zeros.
    while n > 0 && zbuf[z + (n - 1) as usize] == b'0' {
        n -= 1;
    }

    FpDecode {
        sign,
        n: n as usize,
        i_dp,
        digits: zbuf[z..z + n as usize].to_vec(),
        is_special: 0,
    }
}

/// The `g`/`f`/`e` rendering envelope from `printf.c`. `gtype` selects the conversion:
/// `Generic` is `%g`, `Fixed` is `%f`. `altform2` is the `!` flag (round-trip). Always uses the
/// lower-case exponent letter, no width/sign flags (sufficient for the REAL→text and `round`
/// paths).
enum Conv {
    Generic,
    Fixed,
}

fn render(r: f64, precision: i32, gtype: Conv, altform2: bool) -> String {
    let mut precision = precision;
    if precision < 0 {
        precision = 6;
    }
    let i_round = match gtype {
        Conv::Fixed => -precision,
        Conv::Generic => {
            if precision == 0 {
                precision = 1;
            }
            precision
        }
    };
    let s = fp_decode(r, i_round, if altform2 { 20 } else { 16 });

    if s.is_special != 0 {
        if s.is_special == 2 {
            return "NaN".to_string();
        }
        return if s.sign == b'-' { "-Inf" } else { "Inf" }.to_string();
    }

    let prefix = if s.sign == b'-' { "-" } else { "" };
    let exp = s.i_dp - 1;

    // etGENERIC → etEXP or etFLOAT.
    let mut xtype_exp;
    let mut flag_rtz;
    match gtype {
        Conv::Generic => {
            precision -= 1;
            flag_rtz = true; // !alternateform
            if exp < -4 || exp > precision {
                xtype_exp = true;
            } else {
                precision -= exp;
                xtype_exp = false;
            }
        }
        Conv::Fixed => {
            flag_rtz = altform2;
            xtype_exp = false;
        }
    }
    let mut e2 = if xtype_exp { 0 } else { s.i_dp - 1 };
    let flag_dp = precision > 0 || altform2;

    let mut out = String::new();
    out.push_str(prefix);

    // Digits before the decimal point.
    let digits = &s.digits;
    let sn = s.n as i32;
    let mut j: i32 = 0;
    if e2 < 0 {
        out.push('0');
    } else {
        j = e2 + 1;
        if j > sn {
            j = sn;
        }
        for k in 0..j {
            out.push(digits[k as usize] as char);
        }
        e2 -= j;
        if e2 >= 0 {
            for _ in 0..=e2 {
                out.push('0');
            }
            e2 = -1;
        }
    }

    if flag_dp {
        out.push('.');
    }

    // Leading zeros after the decimal point before the first significant digit.
    if e2 < -1 && precision > 0 {
        let mut nn = -1 - e2;
        if nn > precision {
            nn = precision;
        }
        for _ in 0..nn {
            out.push('0');
        }
        precision -= nn;
    }

    // Significant digits after the decimal point.
    if precision > 0 {
        let mut nn = sn - j;
        if nn > precision {
            nn = precision;
        }
        if nn > 0 {
            for k in j..j + nn {
                out.push(digits[k as usize] as char);
            }
            precision -= nn;
        }
        if precision > 0 && !flag_rtz {
            for _ in 0..precision {
                out.push('0');
            }
        }
    }

    // Remove trailing zeros and a bare ".".
    if flag_rtz && flag_dp {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            if altform2 {
                out.push('0');
            } else {
                out.pop();
            }
        }
    }

    // Exponent suffix.
    if xtype_exp {
        let mut exp = s.i_dp - 1;
        out.push('e');
        if exp < 0 {
            out.push('-');
            exp = -exp;
        } else {
            out.push('+');
        }
        if exp >= 100 {
            out.push((b'0' + (exp / 100) as u8) as char);
            exp %= 100;
        }
        out.push((b'0' + (exp / 10) as u8) as char);
        out.push((b'0' + (exp % 10) as u8) as char);
    }
    // Suppress an unused-assignment warning in the Fixed branch.
    let _ = &mut xtype_exp;
    let _ = &mut flag_rtz;
    out
}

/// Render a REAL exactly as `sqlite3_column_text` does (`%!.17g`). This is the canonical
/// REAL → text used by output formatting and by TEXT-affinity coercion.
pub fn fp_to_text(r: f64) -> String {
    render(r, 17, Conv::Generic, true)
}

/// Render a REAL as `%!.*f` with `precision` digits after the decimal point — the formatting
/// `round(X, N)` uses for `N > 0` before parsing the result back to a double.
pub fn fp_to_fixed(r: f64, precision: i32) -> String {
    render(r, precision, Conv::Fixed, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::approx_constant)] // 3.14159 is a deliberate golden test vector, not π
    fn golden_values_match_sqlite3() {
        // Seeded from `sqlite3 :memory: ".mode list" "SELECT <expr>;"` on 3.53.1.
        let cases: &[(f64, &str)] = &[
            (2.0, "2.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e20, "1.0e+20"),
            (1.5e300, "1.5e+300"),
            (3.14159, "3.14159"),
            (1.0 / 3.0, "0.33333333333333332"),
            (0.0, "0.0"),
            (100.0, "100.0"),
            (0.0001, "0.0001"),
            (1e-10, "1.0e-10"),
            (123456789012345.0, "123456789012345.0"),
            (1234567890123456789.0, "1.2345678901234568e+18"),
            (9007199254740992.0, "9007199254740992.0"),
            (0.5, "0.5"),
            (1e16, "10000000000000000.0"),
            (1e15, "1000000000000000.0"),
            (1.0, "1.0"),
            (10.0, "10.0"),
            (0.1, "0.1"),
            (1.5, "1.5"),
            (-2.5, "-2.5"),
            (49.47, "49.47"),
        ];
        for (v, expect) in cases {
            assert_eq!(&fp_to_text(*v), expect, "fp_to_text({v})");
        }
    }

    #[test]
    fn negative_zero_renders_as_zero() {
        assert_eq!(fp_to_text(-0.0), "0.0");
    }

    /// Differential fuzz: render thousands of random doubles and confirm each matches the
    /// system `sqlite3`'s `%!.17g` output. Skips when `sqlite3` is not installed. Each value is
    /// fed to `sqlite3` as a 30-significant-digit decimal (unambiguously rounding back to the
    /// exact double), so this tests rendering equality, not parser round-tripping.
    #[test]
    fn fuzz_matches_sqlite3() {
        use std::process::Command;

        let available = Command::new("sqlite3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !available {
            eprintln!("skipping fp fuzz: system `sqlite3` not found");
            return;
        }

        // splitmix64 PRNG over random bit patterns — deterministic, no rand dependency.
        let mut state: u64 = 0x9e3779b97f4a7c15;
        let mut next = || {
            state = state.wrapping_add(0x9e3779b97f4a7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
            z ^ (z >> 31)
        };

        let n = 4000;
        let mut values = Vec::with_capacity(n);
        let mut sql = String::from(".mode list\n");
        while values.len() < n {
            let d = f64::from_bits(next());
            if !d.is_finite() || d == 0.0 {
                continue;
            }
            // 30 significant digits unambiguously identifies the exact double.
            sql.push_str(&format!("SELECT {:.30e};\n", d));
            values.push(d);
        }

        let mut tmp = std::env::temp_dir();
        tmp.push(format!("rustsqlite_fp_fuzz_{}.sql", std::process::id()));
        std::fs::write(&tmp, &sql).expect("write sql");
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!(".read {}", tmp.display()))
            .output()
            .expect("run sqlite3");
        let _ = std::fs::remove_file(&tmp);
        assert!(out.status.success(), "sqlite3 failed");
        let text = String::from_utf8(out.stdout).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), values.len(), "line count mismatch");

        let mut mismatches = 0;
        for (d, expected) in values.iter().zip(lines.iter()) {
            let got = fp_to_text(*d);
            if got != *expected {
                if mismatches < 10 {
                    eprintln!(
                        "mismatch for bits {:#018x}: got {got}, sqlite3 {expected}",
                        d.to_bits()
                    );
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches, 0,
            "{mismatches}/{n} renderings differ from sqlite3"
        );
    }
}
