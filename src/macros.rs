#![macro_use]

// Don't use these macros any more
// TODO(pc) Remove these macros altogether

#[macro_export]
macro_rules! try_str {
    ($expr:expr) => {
        try!(($expr).map_err(|e| format!("{}: {}", stringify!($expr), e)))
    }
}

#[macro_export]
macro_rules! try_opt {
    ($expr:expr) => {
        match $expr {
            Some(x) => x,
            None => return None,
        }
    }
}

