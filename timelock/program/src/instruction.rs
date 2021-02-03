use std::{convert::TryInto, mem::size_of};

use arrayref::array_refs;
use solana_program::program_error::ProgramError;

use crate::{
    error::TimelockError,
    state::{
        custom_single_signer_timelock_transaction::INSTRUCTION_LIMIT, enums::ConsensusAlgorithm,
        enums::ExecutionType, enums::TimelockType, timelock_config::TimelockConfig,
    },
};

/// Used for telling caller what type of format you want back
#[derive(Clone, Debug, PartialEq)]
pub enum Format {
    /// JSON format
    JSON,
    /// MsgPack format
    MsgPack,
}
impl Default for Format {
    fn default() -> Self {
        Format::JSON
    }
}

/// Instructions supported by the Timelock program.
#[derive(Clone, Debug, PartialEq)]
pub enum TimelockInstruction {
    /// Initializes a new Timelock Program.
    ///
    ///   0. `[writable]` Timelock program account pub key.
    ///   1. `[]` Token program id
    ///   2. `[]` Rent sysvar
    InitTimelockProgram,

    /// Initializes a new empty Timelocked set of Instructions that will be executed at various slots in the future in draft mode.
    /// Grants Admin token to caller.
    ///
    ///   0. `[writable]` Uninitialized Timelock set account .
    ///   1. `[writable]` Uninitialized Signatory Mint account
    ///   2. `[writable]` Uninitialized Admin Mint account
    ///   3. `[writable]` Uninitialized Voting Mint account
    ///   4. `[writable]` Uninitialized Signatory Validation account
    ///   5. `[writable]` Uninitialized Admin Validation account
    ///   6. `[writable]` Uninitialized Voting Validation account
    ///   7. `[writable]` Uninitialized Destination account for first admin token
    ///   8. `[writable]` Uninitialized Destination account for first signatory token
    ///   9. `[]` Timelock Program
    ///   10. `[]` Rent sysvar
    ///   11. '[]` Token program id
    InitTimelockSet {
        /// Determine what type of timelock config you want
        config: TimelockConfig,
    },

    /// [Requires Admin token]
    /// Adds a signatory to the Timelock which means that this timelock can't leave Draft state until yet another signatory burns
    /// their signatory token indicating they are satisfied with the instruction queue. They'll receive an signatory token
    /// as a result of this call that they can burn later.
    ///
    ///   0. `[writable]` Uninitialized new signatory account.
    ///   1. `[writable]` Signatory mint account.
    ///   2. `[writable]` Admin account.
    ///   3. `[]` Admin validation account.
    ///   4. `[]` Timelock set account.
    ///   5. `[]` Timelock program account.
    ///   6. `[]` Rent sysvar
    ///   7. '[]` Token program id.
    AddSigner,

    /// [Requires Admin token]
    /// Removes a signer from the set.
    ///
    ///   0. `[writable]` Signatory account to remove token from.
    ///   1. `[writable]` Signatory mint account.
    ///   2. `[writable]` Admin account.
    ///   3. `[]` Admin validation account.
    ///   4. `[]` Timelock set account.
    ///   5. `[]` Timelock program account.
    ///   6. '[]` Token program id.
    RemoveSigner,

    /// [Requires Signatory token]
    /// Adds a Transaction to the Timelock Set. Max of 10 of any Transaction type. More than 10 will throw error.
    /// Creates a PDA using your authority to be used to later execute the instruction.
    /// This transaction needs to contain authority to execute the program.
    ///
    ///   0. `[writable]` Timelock set account.
    ///   1. `[writable]` Uninitialized Timelock Transaction account.
    ///   2. `[writable]` Signatory account
    ///   3. `[]` Signatory validation account account.
    ///   4. `[]` Timelock program account.
    ///   5. `[]` Token program account.
    AddCustomSingleSignerTransaction {
        /// Slot during which this will run
        slot: u64,
        /// Instruction
        instruction: [u8; INSTRUCTION_LIMIT],
        /// Position in transaction array
        position: u8,
    },

    /// [Requires Signatory token]
    /// Remove Transaction from the Timelock Set.
    ///
    ///   0. `[writable]` Timelock set account pub key.
    ///   1. `[writable]` Timelock Transaction pub key.
    ///   2. `[]` Timelock program account pub key.
    RemoveTransaction {},

    /// [Requires Signatory token]
    /// Update Transaction slot in the Timelock Set. Useful during reset periods.
    ///
    ///   0. `[writable]` Timelock Transaction pub key.
    ///   1. `[]` Timelock set account pub key.
    ///   1. `[]` Timelock program account pub key.
    UpdateTransactionSlot {
        /// On what slot this transaction slot will now run
        slot: u64,
    },

    /// [Requires Admin token]
    /// Delete Timelock set entirely.
    ///
    ///   0. `[writable]` Timelock set account pub key.
    ///   1. `[]` Timelock program account pub key.
    DeleteTimelockSet {},

    /// [Requires Signatory token]
    /// Burns signatory token, indicating you approve of moving this Timelock set from Draft state to Voting state.
    /// The last Signatory token to be burned moves the state to Voting.
    ///
    ///   0. `[writable]` Timelock set account pub key.
    ///   1. `[]` Timelock program account pub key.
    Sign {},

    /// [Requires Voting tokens]
    /// Burns voting tokens, indicating you approve of running this set of transactions. If you tip the consensus,
    /// then the transactions begin to be run at their time slots.
    ///
    ///   0. `[]` Timelock set account pub key.
    ///   1. `[]` Timelock program account pub key.
    Vote {
        /// How many voting tokens to burn
        voting_token_amount: u64,
    },

    /// [Requires Signatory token]
    /// Mints voting tokens for a destination account to be used during the voting process.
    ///
    ///   0. `[writable]` Timelock set account pub key.
    ///   1. `[]` Timelock program account pub key.
    ///   2. `[]` Destination account pub key.
    MintVotingTokens {
        /// How many voting tokens to mint
        voting_token_amount: u64,
    },
}

impl TimelockInstruction {
    /// Unpacks a byte buffer into a [TimelockInstruction](enum.TimelockInstruction.html).
    pub fn unpack(input: &[u8]) -> Result<Self, ProgramError> {
        let (&tag, rest) = input
            .split_first()
            .ok_or(TimelockError::InstructionUnpackError)?;
        Ok(match tag {
            0 => Self::InitTimelockProgram,
            1 => Self::InitTimelockSet {
                config: TimelockConfig {
                    consensus_algorithm: match input[0] {
                        0 => ConsensusAlgorithm::Majority,
                        1 => ConsensusAlgorithm::SuperMajority,
                        2 => ConsensusAlgorithm::FullConsensus,
                        _ => ConsensusAlgorithm::Majority,
                    },
                    execution_type: match input[1] {
                        0 => ExecutionType::AllOrNothing,
                        1 => ExecutionType::AnyAboveVoteFinishSlot,
                        _ => ExecutionType::AllOrNothing,
                    },
                    timelock_type: match input[2] {
                        0 => TimelockType::CustomSingleSignerV1,
                        _ => TimelockType::CustomSingleSignerV1,
                    },
                },
            },
            2 => Self::AddSigner,
            3 => Self::RemoveSigner,
            4 => {
                let (slot, rest) = Self::unpack_u64(rest)?;
                let (instruction, rest) = Self::unpack_instructions(rest)?;
                let (position, rest) = Self::unpack_u8(rest)?;
                Self::AddCustomSingleSignerTransaction {
                    slot,
                    instruction,
                    position,
                }
            }
            _ => return Err(TimelockError::InstructionUnpackError.into()),
        })
    }

    fn unpack_u64(input: &[u8]) -> Result<(u64, &[u8]), ProgramError> {
        if input.len() >= 8 {
            let (amount, rest) = input.split_at(8);
            let amount = amount
                .get(..8)
                .and_then(|slice| slice.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or(TimelockError::InstructionUnpackError)?;
            Ok((amount, rest))
        } else {
            Err(TimelockError::InstructionUnpackError.into())
        }
    }

    fn unpack_instructions(input: &[u8]) -> Result<([u8; INSTRUCTION_LIMIT], &[u8]), ProgramError> {
        if !input.is_empty() {
            if input.len() < INSTRUCTION_LIMIT {
                return Err(TimelockError::InstructionUnpackError.into());
            }

            let (input_instruction, rest) = input.split_at(INSTRUCTION_LIMIT + 1);
            let mut instruction: [u8; INSTRUCTION_LIMIT] = [0; INSTRUCTION_LIMIT];
            for n in 0..(INSTRUCTION_LIMIT - 1) {
                instruction[n] = input_instruction[n];
            }
            Ok((instruction, rest))
        } else {
            Err(TimelockError::InstructionUnpackError.into())
        }
    }

    fn unpack_u8(input: &[u8]) -> Result<(u8, &[u8]), ProgramError> {
        if !input.is_empty() {
            let (amount, rest) = input.split_at(1);
            let amount = amount
                .get(..1)
                .and_then(|slice| slice.try_into().ok())
                .map(u8::from_le_bytes)
                .ok_or(TimelockError::InstructionUnpackError)?;
            Ok((amount, rest))
        } else {
            Err(TimelockError::InstructionUnpackError.into())
        }
    }

    /// Packs a [TimelockInstruction](enum.TimelockInstruction.html) into a byte buffer.
    pub fn pack(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(size_of::<Self>());

        match self {
            Self::InitTimelockProgram => {
                buf.push(0);
            }
            Self::InitTimelockSet { config } => {
                buf.push(1);
                match config.consensus_algorithm {
                    ConsensusAlgorithm::Majority => buf.push(0),
                    ConsensusAlgorithm::SuperMajority => buf.push(1),
                    ConsensusAlgorithm::FullConsensus => buf.push(2),
                }
                match config.execution_type {
                    ExecutionType::AllOrNothing => buf.push(0),
                    ExecutionType::AnyAboveVoteFinishSlot => buf.push(1),
                }
                match config.timelock_type {
                    TimelockType::CustomSingleSignerV1 => buf.push(0),
                }
            }
            Self::AddSigner => buf.push(2),
            Self::RemoveSigner => buf.push(3),
            Self::AddCustomSingleSignerTransaction {
                slot,
                instruction,
                position,
            } => {
                buf.push(4);
                buf.extend_from_slice(&slot.to_le_bytes());
                buf.extend_from_slice(instruction);
                buf.extend_from_slice(&position.to_le_bytes());
            }
            Self::RemoveTransaction {} => {}
            Self::UpdateTransactionSlot { slot } => {}
            Self::DeleteTimelockSet {} => {}
            Self::Sign {} => {}
            Self::Vote {
                voting_token_amount,
            } => {}
            Self::MintVotingTokens {
                voting_token_amount,
            } => {}
        }
        buf
    }
}
