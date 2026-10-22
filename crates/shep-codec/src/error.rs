/// Why bytes couldn't be decoded as the requested type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The input ended, or an envelope's body ended, mid-value.
    #[error("unexpected end of input: needed {needed} bytes, {remaining} remain")]
    UnexpectedEnd { needed: usize, remaining: usize },

    /// A `bool` or an `Option` presence flag was neither 0 nor 1.
    #[error("invalid bool: {0}")]
    InvalidBool(u8),

    /// A string's bytes weren't UTF-8.
    #[error("invalid UTF-8 in string")]
    InvalidUtf8,

    /// An enum's tag names no variant this definition knows.
    #[error("unknown tag {tag} for {type_name}")]
    UnknownTag { type_name: &'static str, tag: u8 },

    /// The envelope's versions say this definition can't read it: either its
    /// compat version is above our version, or its version is below our compat
    /// version.
    #[error(
        "incompatible {type_name}: data version {version} (compat {compat_version}), \
         reader version {reader_version} (compat {reader_compat_version})"
    )]
    IncompatibleVersion {
        type_name: &'static str,
        version: u8,
        compat_version: u8,
        reader_version: u8,
        reader_compat_version: u8,
    },

    /// The envelope ended before a field that has no default.
    #[error("{type_name} is missing field `{field}`")]
    MissingField {
        type_name: &'static str,
        field: &'static str,
    },

    /// A map or set held the same key twice.
    #[error("duplicate entry in {type_name}")]
    DuplicateEntry { type_name: &'static str },

    /// Derived types were nested deeper than [`MAX_DEPTH`](crate::MAX_DEPTH).
    #[error("nesting exceeds the maximum depth")]
    TooDeep,

    /// [`from_bytes`](crate::from_bytes) decoded a value with input left over.
    #[error("{0} trailing bytes")]
    TrailingBytes(usize),

    /// A hand-written [`Decode`](crate::Decode) rejected the input.
    #[error("{0}")]
    Custom(String),
}
