//! Custom error codes. Surfaced as `ProgramError::Custom(code)`.

use solana_program::program_error::ProgramError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MockSyError {
    UnknownDiscriminator = 0,
    InvalidInstructionData = 1,
    InvalidPda = 2,
    AccountNotInitialized = 3,
    AccountAlreadyInitialized = 4,
    AccountTooSmall = 5,
    SerializationFailed = 6,
    DeserializationFailed = 7,
    MathOverflow = 8,
    NumberTooLarge = 9,
    ZeroExchangeRate = 10,
    EmissionIndexOutOfRange = 11,
    TooManyEmissions = 12,
    UnknownEmissionMint = 13,
    InsufficientClaimable = 14,
    InsufficientSyBalance = 15,
    MissingSigner = 16,
    WrongAccountOwner = 17,
}

impl From<MockSyError> for ProgramError {
    fn from(e: MockSyError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
