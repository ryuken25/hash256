use eyre::Report;

pub type Result<T> = std::result::Result<T, Report>;
