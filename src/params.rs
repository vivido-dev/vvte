//! Fixed size parameters list with optional subparameters.

use core::fmt::{self, Debug, Formatter, Write};

pub(crate) const MAX_PARAMS: usize = 32;

/// The numeric parameters of a CSI or DCS sequence.
///
/// Each parameter is a list of one or more values: `;` separates parameters and `:` separates a
/// parameter's subparameters, so `CSI 1;38:2:255:0:0 m` has two parameters, `[1]` and
/// `[38, 2, 255, 0, 0]`. An omitted value is `0`, and values saturate at [`u16::MAX`].
///
/// At most 32 values are kept per sequence, counting subparameters; the parser reports a
/// sequence with more as ignored.
///
/// # Examples
///
/// ```
/// use vvte::{Params, Parser, Perform};
///
/// #[derive(Default)]
/// struct Collect(Vec<Vec<u16>>, usize);
///
/// impl Perform for Collect {
///     fn csi_dispatch(&mut self, params: &Params, _: &[u8], _: bool, _: char) {
///         self.0 = params.iter().map(<[u16]>::to_vec).collect();
///         self.1 = params.len();
///     }
/// }
///
/// let mut parser: Parser = Parser::default();
/// let mut collect = Collect::default();
/// parser.advance(&mut collect, b"\x1b[1;38:2:255:0:0m");
/// assert_eq!(collect.0, [vec![1], vec![38, 2, 255, 0, 0]]);
/// assert_eq!(collect.1, 6, "len counts every value, subparameters included");
/// ```
#[derive(Default)]
pub struct Params {
    /// Number of subparameters for each parameter.
    ///
    /// For each entry in the `values` slice, this stores the length of the
    /// param as number of subparams at the same index as the param in the
    /// `values` slice.
    ///
    /// At the subparam positions the length will always be `0`.
    subparams: [u8; MAX_PARAMS],

    /// All parameters and subparameters.
    values: [u16; MAX_PARAMS],

    /// Number of subparameters in the current parameter.
    current_subparams: u8,

    /// Total number of parameters and subparameters.
    len: usize,
}

impl Params {
    /// Returns the number of values, counting each subparameter separately.
    ///
    /// For `CSI 1;38:2:255:0:0 m` this is 6. The number of parameters is
    /// `self.iter().count()`, which is 2.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if there are no parameters present.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns an iterator over the parameters, each as a slice of its values.
    #[inline]
    #[must_use]
    pub fn iter(&self) -> ParamsIter<'_> {
        ParamsIter::new(self)
    }

    /// Returns `true` if there is no more space for additional parameters.
    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.len == MAX_PARAMS
    }

    /// Clear all parameters.
    #[inline]
    pub(crate) fn clear(&mut self) {
        self.current_subparams = 0;
        self.len = 0;
    }

    /// Add an additional parameter.
    #[inline]
    pub(crate) fn push(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.values[self.len] = item;
        self.current_subparams = 0;
        self.len += 1;
    }

    /// Add an additional subparameter to the current parameter.
    #[inline]
    pub(crate) fn extend(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.values[self.len] = item;
        self.current_subparams += 1;
        self.len += 1;
    }
}

impl<'a> IntoIterator for &'a Params {
    type IntoIter = ParamsIter<'a>;
    type Item = &'a [u16];

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over the parameters of a [`Params`], each as a slice of its values.
#[derive(Debug)]
pub struct ParamsIter<'a> {
    params: &'a Params,
    index: usize,
}

impl<'a> ParamsIter<'a> {
    #[inline]
    fn new(params: &'a Params) -> Self {
        Self { params, index: 0 }
    }
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.params.len() {
            return None;
        }

        // Get all subparameters for the current parameter.
        let num_subparams = self.params.subparams[self.index];
        let param = &self.params.values[self.index..self.index + num_subparams as usize];

        // Jump to the next parameter.
        self.index += num_subparams as usize;

        Some(param)
    }

    /// Every remaining value could belong to one parameter, or each could be its own.
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.params.len() - self.index;
        (usize::from(remaining != 0), Some(remaining))
    }
}

impl Debug for Params {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_char('[')?;

        for (i, param) in self.iter().enumerate() {
            if i != 0 {
                f.write_char(';')?;
            }

            for (i, subparam) in param.iter().enumerate() {
                if i != 0 {
                    f.write_char(':')?;
                }

                subparam.fmt(f)?;
            }
        }

        f.write_char(']')
    }
}
