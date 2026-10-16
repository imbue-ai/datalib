//! A unit, written as a UCUM code (the Unified Code for Units of
//! Measure, <https://ucum.org/ucum>): `m/s2`, `kg/m2`, `{beats}/min`,
//! `Cel`. HL7 FHIR and OpenTelemetry write units this way. The parser
//! takes the grammar whole — prefixes, `.` and `/`, integer exponents,
//! parentheses, `{annotations}` and integer factors — over the atoms in
//! [`ATOMS`]; a code naming any other atom is refused, and an atom is
//! added there when a source needs it.

use std::fmt;

/// The base quantities, in the order a [`Unit`]'s exponents are kept:
/// length, mass, time, current, temperature, amount, luminosity.
const BASES: [&str; 7] = ["L", "M", "T", "I", "Θ", "N", "J"];

#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    code: String,
    label: String,
    dims: [i8; 7],
    /// A value times this, plus `offset`, is in the coherent SI unit of
    /// its dimension (kg, not g).
    factor: f64,
    offset: f64,
    /// Sorted. UCUM gives an annotation no meaning; we keep two units
    /// that differ only in one (`{steps}`, `{score}`) apart, since
    /// nobody wants them on one axis.
    annotations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitError(String);

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnitError {}

struct Atom {
    code: &'static str,
    label: &'static str,
    /// Whether a prefix may come before it (`km`, `mL`, `kcal`).
    metric: bool,
    factor: f64,
    dims: [i8; 7],
    /// Set for the units on an offset scale (°C, °F), which UCUM allows
    /// only alone.
    offset: Option<f64>,
}

const fn atom(
    code: &'static str,
    label: &'static str,
    metric: bool,
    factor: f64,
    dims: [i8; 7],
) -> Atom {
    Atom {
        code,
        label,
        metric,
        factor,
        dims,
        offset: None,
    }
}

const NONE: [i8; 7] = [0, 0, 0, 0, 0, 0, 0];
const LENGTH: [i8; 7] = [1, 0, 0, 0, 0, 0, 0];
const MASS: [i8; 7] = [0, 1, 0, 0, 0, 0, 0];
const TIME: [i8; 7] = [0, 0, 1, 0, 0, 0, 0];
const ENERGY: [i8; 7] = [2, 1, -2, 0, 0, 0, 0];
const PRESSURE: [i8; 7] = [-1, 1, -2, 0, 0, 0, 0];
const TEMPERATURE: [i8; 7] = [0, 0, 0, 0, 1, 0, 0];

#[rustfmt::skip]
const ATOMS: &[Atom] = &[
    atom("m", "m", true, 1.0, LENGTH),
    atom("g", "g", true, 1e-3, MASS),
    atom("s", "s", true, 1.0, TIME),
    atom("A", "A", true, 1.0, [0, 0, 0, 1, 0, 0, 0]),
    atom("K", "K", true, 1.0, TEMPERATURE),
    atom("mol", "mol", true, 1.0, [0, 0, 0, 0, 0, 1, 0]),
    atom("cd", "cd", true, 1.0, [0, 0, 0, 0, 0, 0, 1]),
    atom("rad", "rad", true, 1.0, NONE),
    atom("sr", "sr", true, 1.0, NONE),
    atom("Hz", "Hz", true, 1.0, [0, 0, -1, 0, 0, 0, 0]),
    atom("N", "N", true, 1.0, [1, 1, -2, 0, 0, 0, 0]),
    atom("Pa", "Pa", true, 1.0, PRESSURE),
    atom("J", "J", true, 1.0, ENERGY),
    atom("W", "W", true, 1.0, [2, 1, -3, 0, 0, 0, 0]),
    atom("C", "C", true, 1.0, [0, 0, 1, 1, 0, 0, 0]),
    atom("V", "V", true, 1.0, [2, 1, -3, -1, 0, 0, 0]),
    atom("Ohm", "Ω", true, 1.0, [2, 1, -3, -2, 0, 0, 0]),
    atom("L", "L", true, 1e-3, [3, 0, 0, 0, 0, 0, 0]),
    atom("l", "L", true, 1e-3, [3, 0, 0, 0, 0, 0, 0]),
    atom("t", "t", true, 1e3, MASS),
    atom("bar", "bar", true, 1e5, PRESSURE),
    atom("cal", "cal", true, 4.184, ENERGY),
    atom("m[Hg]", "mHg", true, 133_322.0, PRESSURE),
    Atom { offset: Some(273.15), ..atom("Cel", "°C", false, 1.0, TEMPERATURE) },
    Atom { offset: Some(459.67 * 5.0 / 9.0), ..atom("[degF]", "°F", false, 5.0 / 9.0, TEMPERATURE) },
    atom("min", "min", false, 60.0, TIME),
    atom("h", "h", false, 3_600.0, TIME),
    atom("d", "d", false, 86_400.0, TIME),
    atom("wk", "wk", false, 604_800.0, TIME),
    atom("mo", "mo", false, 2_629_800.0, TIME),
    atom("a", "yr", false, 31_557_600.0, TIME),
    atom("deg", "°", false, std::f64::consts::PI / 180.0, NONE),
    atom("%", "%", false, 1e-2, NONE),
    atom("[ppm]", "ppm", false, 1e-6, NONE),
    atom("[ppb]", "ppb", false, 1e-9, NONE),
    atom("[Cal]", "Cal", false, 4_184.0, ENERGY),
    atom("[lb_av]", "lb", false, 0.453_592_37, MASS),
    atom("[oz_av]", "oz", false, 0.028_349_523_125, MASS),
    atom("[in_i]", "in", false, 0.0254, LENGTH),
    atom("[ft_i]", "ft", false, 0.3048, LENGTH),
    atom("[yd_i]", "yd", false, 0.9144, LENGTH),
    atom("[mi_i]", "mi", false, 1_609.344, LENGTH),
    atom("[gal_us]", "gal", false, 3.785_411_784e-3, [3, 0, 0, 0, 0, 0, 0]),
];

/// Longest first, so `da` is tried before `d`.
#[rustfmt::skip]
const PREFIXES: &[(&str, &str, f64)] = &[
    ("da", "da", 1e1),
    ("Y", "Y", 1e24), ("Z", "Z", 1e21), ("E", "E", 1e18), ("P", "P", 1e15),
    ("T", "T", 1e12), ("G", "G", 1e9), ("M", "M", 1e6), ("k", "k", 1e3),
    ("h", "h", 1e2), ("d", "d", 1e-1), ("c", "c", 1e-2), ("m", "m", 1e-3),
    ("u", "µ", 1e-6), ("n", "n", 1e-9), ("p", "p", 1e-12), ("f", "f", 1e-15),
    ("a", "a", 1e-18), ("z", "z", 1e-21), ("y", "y", 1e-24),
];

impl Unit {
    pub fn parse(code: &str) -> Result<Unit, UnitError> {
        let mut p = Parser {
            s: code.as_bytes(),
            i: 0,
            specials: 0,
        };
        let term = p.term()?;
        if p.i != p.s.len() {
            return Err(p.error("an operator or the end"));
        }
        if p.specials > 0 && term.offset.is_none() {
            return Err(UnitError(format!(
                "{code:?}: °C and °F cannot be combined with another unit, prefixed or raised \
                 to a power"
            )));
        }
        let mut annotations = term.annotations;
        annotations.sort();
        Ok(Unit {
            code: code.to_string(),
            label: term.label,
            dims: term.dims,
            factor: term.factor,
            offset: term.offset.unwrap_or(0.0),
            annotations,
        })
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// The unit as a chart's axis shows it: `m/s²`, `beats/min`, `°C`.
    /// Empty for a pure number.
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn factor(&self) -> f64 {
        self.factor
    }

    pub fn offset(&self) -> f64 {
        self.offset
    }

    /// What two units must share for a value in one to be shown in the
    /// other: the powers of the base quantities, then the annotations.
    /// `L1.T-2`, `T-1{beats}`; empty for a pure number.
    pub fn quantity(&self) -> String {
        let dims = BASES
            .iter()
            .zip(self.dims)
            .filter(|(_, e)| *e != 0)
            .map(|(b, e)| format!("{b}{e}"))
            .collect::<Vec<_>>()
            .join(".");
        let notes: String = self
            .annotations
            .iter()
            .map(|a| format!("{{{a}}}"))
            .collect();
        format!("{dims}{notes}")
    }

    /// `value` in this unit, in `to`; `None` when they measure different
    /// quantities.
    pub fn convert(&self, value: f64, to: &Unit) -> Option<f64> {
        (self.quantity() == to.quantity())
            .then(|| (value * self.factor + self.offset - to.offset) / to.factor)
    }
}

/// A parsed term, before it is a [`Unit`].
struct Term {
    label: String,
    dims: [i8; 7],
    factor: f64,
    /// `Some` only for a term that is one offset-scale unit, alone.
    offset: Option<f64>,
    annotations: Vec<String>,
}

impl Term {
    fn one() -> Self {
        Term {
            label: String::new(),
            dims: NONE,
            factor: 1.0,
            offset: None,
            annotations: Vec::new(),
        }
    }

    fn times(mut self, other: Term, sign: i8) -> Self {
        for (d, o) in self.dims.iter_mut().zip(other.dims) {
            *d += sign * o;
        }
        self.factor *= other.factor.powi(sign.into());
        self.annotations.extend(other.annotations);
        self.offset = None;
        self
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    /// How many offset-scale atoms were read: each must be the whole code.
    specials: usize,
}

impl Parser<'_> {
    fn error(&self, wanted: &str) -> UnitError {
        let code = String::from_utf8_lossy(self.s);
        UnitError(format!(
            "{code:?} is not a unit this build reads: expected {wanted} at position {}",
            self.i
        ))
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn term(&mut self) -> Result<Term, UnitError> {
        let (mut term, mut label) = if self.peek() == Some(b'/') {
            self.i += 1;
            let c = self.component()?;
            let label = format!("/{}", c.label);
            (Term::one().times(c, -1), label)
        } else {
            let c = self.component()?;
            let label = c.label.clone();
            (c, label)
        };
        while let Some(op @ (b'.' | b'/')) = self.peek() {
            self.i += 1;
            let c = self.component()?;
            label.push_str(if op == b'.' { "·" } else { "/" });
            label.push_str(&c.label);
            term = term.times(c, if op == b'.' { 1 } else { -1 });
        }
        term.label = label;
        Ok(term)
    }

    fn component(&mut self) -> Result<Term, UnitError> {
        match self.peek() {
            Some(b'(') => {
                self.i += 1;
                let inner = self.term()?;
                if self.peek() != Some(b')') {
                    return Err(self.error("`)`"));
                }
                self.i += 1;
                Ok(Term {
                    label: format!("({})", inner.label),
                    offset: None,
                    ..inner
                })
            }
            Some(b'{') => {
                let note = self.annotation()?;
                Ok(Term {
                    label: note.clone(),
                    annotations: vec![note],
                    ..Term::one()
                })
            }
            Some(b'0'..=b'9') => {
                let start = self.i;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
                let digits = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("1");
                let factor: f64 = digits.parse().map_err(|_| self.error("a number"))?;
                let label = if factor == 1.0 {
                    String::new()
                } else {
                    digits.to_string()
                };
                let mut t = Term {
                    label,
                    factor,
                    ..Term::one()
                };
                if self.peek() == Some(b'{') {
                    let note = self.annotation()?;
                    t.label = note.clone();
                    t.annotations.push(note);
                }
                Ok(t)
            }
            _ => self.simple(),
        }
    }

    fn annotation(&mut self) -> Result<String, UnitError> {
        let start = self.i + 1;
        let Some(len) = self.s[start..].iter().position(|&c| c == b'}') else {
            return Err(self.error("`}`"));
        };
        self.i = start + len + 1;
        String::from_utf8(self.s[start..start + len].to_vec()).map_err(|_| self.error("text"))
    }

    fn simple(&mut self) -> Result<Term, UnitError> {
        let start = self.i;
        while let Some(c) = self.peek() {
            match c {
                b'[' => match self.s[self.i..].iter().position(|&c| c == b']') {
                    Some(len) => self.i += len + 1,
                    None => return Err(self.error("`]`")),
                },
                b'a'..=b'z' | b'A'..=b'Z' | b'%' => self.i += 1,
                _ => break,
            }
        }
        if self.i == start {
            return Err(self.error("a unit"));
        }
        let name = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.error("a unit"))?;
        let (prefix, atom) = resolve(name).ok_or_else(|| {
            UnitError(format!(
                "{:?}: {name:?} is not a unit this build knows",
                String::from_utf8_lossy(self.s)
            ))
        })?;
        let exp = self.exponent()?;
        if atom.offset.is_some() {
            self.specials += 1;
        }
        let (prefix_label, prefix_factor) = prefix.map_or(("", 1.0), |(_, l, f)| (*l, *f));
        let mut t = Term {
            label: format!("{prefix_label}{}{}", atom.label, superscript(exp)),
            dims: atom.dims.map(|d| d * exp),
            factor: (prefix_factor * atom.factor).powi(exp.into()),
            offset: atom.offset.filter(|_| prefix.is_none() && exp == 1),
            annotations: Vec::new(),
        };
        if self.peek() == Some(b'{') {
            let note = self.annotation()?;
            t.label = format!("{} {note}", t.label);
            t.annotations.push(note);
        }
        Ok(t)
    }

    fn exponent(&mut self) -> Result<i8, UnitError> {
        let start = self.i;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.i += 1;
        }
        let digits = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        if self.i == digits {
            if digits != start {
                return Err(self.error("digits after the sign"));
            }
            return Ok(1);
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|s| s.parse::<i8>().ok())
            .filter(|e| (-9..=9).contains(e))
            .ok_or_else(|| self.error("an exponent from -9 to 9"))
    }
}

type Prefix = (&'static str, &'static str, f64);

fn resolve(name: &str) -> Option<(Option<&'static Prefix>, &'static Atom)> {
    if let Some(a) = ATOMS.iter().find(|a| a.code == name) {
        return Some((None, a));
    }
    PREFIXES.iter().find_map(|p| {
        let rest = name.strip_prefix(p.0)?;
        let a = ATOMS.iter().find(|a| a.code == rest && a.metric)?;
        Some((Some(p), a))
    })
}

fn superscript(exp: i8) -> String {
    if exp == 1 {
        return String::new();
    }
    exp.to_string()
        .chars()
        .map(|c| match c {
            '-' => '⁻',
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            _ => '⁹',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(code: &str) -> Unit {
        Unit::parse(code).unwrap_or_else(|e| panic!("{e}"))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
    }

    #[test]
    fn composite_units_reduce_to_base_quantities() {
        assert_eq!(u("m/s2").quantity(), "L1.T-2");
        assert_eq!(u("kg/m2").quantity(), "L-2.M1");
        assert_eq!(u("N").quantity(), u("kg.m/s2").quantity());
        assert_eq!(u("mL/kg/min").quantity(), "L3.M-1.T-1");
        assert_eq!(u("/min").quantity(), "T-1");
        assert_eq!(u("s-1").quantity(), "T-1");
        assert_eq!(u("m/(s.s)").quantity(), "L1.T-2");
        assert_eq!(u("1").quantity(), "");
        assert_eq!(u("%").quantity(), "");
    }

    #[test]
    fn prefixes_scale_and_exact_atoms_win_over_a_prefix_reading() {
        assert!(close(u("km").factor(), 1_000.0));
        assert!(close(u("ms").factor(), 1e-3));
        assert!(close(u("kg").factor(), 1.0));
        assert!(close(u("kcal").factor(), 4_184.0));
        assert!(close(u("cm2").factor(), 1e-4));
        assert!(close(u("min").factor(), 60.0));
        assert!(close(u("mm[Hg]").factor(), 133.322));
        assert!(close(u("hPa").factor(), 100.0));
        assert!(close(u("dam").factor(), 10.0));
        assert_eq!(u("cd").quantity(), "J1");
        assert_eq!(u("mol").quantity(), "N1");
    }

    #[test]
    fn annotations_keep_counts_of_different_things_apart() {
        assert_eq!(u("{beats}/min").quantity(), "T-1{beats}");
        assert_ne!(u("{steps}").quantity(), u("{score}").quantity());
        assert_ne!(u("{beats}/min").quantity(), u("/min").quantity());
        assert_eq!(u("{beats}/min").label(), "beats/min");
        assert_eq!(u("kg{total}").quantity(), "M1{total}");
    }

    #[test]
    fn values_convert_within_a_quantity_and_across_offset_scales() {
        assert!(close(u("min").convert(90.0, &u("h")).unwrap(), 1.5));
        assert!(close(u("km/h").convert(36.0, &u("m/s")).unwrap(), 10.0));
        assert!(close(u("Cel").convert(100.0, &u("[degF]")).unwrap(), 212.0));
        assert!(close(u("[degF]").convert(32.0, &u("Cel")).unwrap(), 0.0));
        assert!(close(u("K").convert(273.15, &u("Cel")).unwrap(), 0.0));
        assert_eq!(u("kg").convert(1.0, &u("m")), None);
    }

    #[test]
    fn labels_are_what_an_axis_shows() {
        for (code, label) in [
            ("m/s2", "m/s²"),
            ("kg/m2", "kg/m²"),
            ("Cel", "°C"),
            ("us", "µs"),
            ("kg.m", "kg·m"),
            ("s-1", "s⁻¹"),
            ("1", ""),
            ("{steps}", "steps"),
            ("mm[Hg]", "mmHg"),
            ("a", "yr"),
        ] {
            assert_eq!(u(code).label(), label, "{code}");
        }
    }

    #[test]
    fn what_is_not_ucum_is_refused() {
        for bad in [
            "", "bpm", "°C", "kg/", "m/(s", "{beats", "kmin", "Cel/s", "kCel", "Cel2", "m^2",
            "[foo]", "m-", "brpm",
        ] {
            assert!(Unit::parse(bad).is_err(), "{bad:?} parsed");
        }
    }
}
