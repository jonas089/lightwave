// This is the main service that orchestrates the light client update process.
// It manages the state of the light client, generates and verifies proofs,
// and maintains a chain of trusted state transitions.

use std::{fs::write, path::Path, time::Instant};

use alloy_sol_types::SolType;
use anyhow::{Context, Result};
use beacon_electra::{
    extract_electra_block_body, get_beacon_block_header, get_electra_block,
    types::electra::ElectraBlockHeader,
};
use clap::Parser;
use preprocessor::Preprocessor;
use recursion_types::{RecursionCircuitInputs, RecursionCircuitOutputs, WrapperCircuitInputs};
mod helpers;
use sp1_helios_primitives::types::{ProofInputs as HeliosInputs, ProofOutputs as HeliosOutputs};
use sp1_sdk::{HashableKey, ProverClient, SP1Stdin, include_elf};
mod preprocessor;
mod state;
use state::StateManager;
use tree_hash::TreeHash;

/// Command line arguments for the service
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Delete the state file before starting
    #[arg(long)]
    delete: bool,

    /// Initial slot number to start from (only used when initializing new state)
    #[arg(long, default_missing_value = "7606080", num_args(0..=1))]
    generate_recursion_circuit: Option<u64>,

    /// Generate the wrapper circuit
    #[arg(long)]
    generate_wrapper_circuit: bool,

    /// dump the elfs as bytes
    #[arg(long)]
    dump_elfs: bool,
}

// Binary artifacts for the various circuits used in the light client
pub const HELIOS_ELF: &[u8] = include_bytes!("../../elfs/constant/sp1-helios-elf");
pub const RECURSIVE_ELF_RUNTIME: &[u8] = include_elf!("recursion-circuit");
pub const WRAPPER_ELF_RUNTIME: &[u8] = include_elf!("wrapper-circuit");

/// Main entry point for the light client service.
///
/// This function:
/// 1. Initializes the service state with a trusted slot
/// 2. Sets up the prover client and circuit artifacts
/// 3. Enters a loop that:
///    - Generates Helios proofs for new blocks
///    - Verifies proofs recursively
///    - Updates the service state with new trusted information
///    - Commits execution block height and state root instead of beacon header
#[tokio::main]
async fn main() -> Result<()> {
    let mut active_committee_hash: [u8; 32] = [0; 32];
    let start_time = Instant::now();
    // Parse command line arguments
    let args = Args::parse();
    // Load environment variables and initialize the prover client
    dotenvy::dotenv().ok();
    let consensus_url = std::env::var("SOURCE_CONSENSUS_RPC_URL").unwrap_or_default();
    let db_path =
        std::env::var("SERVICE_STATE_DB_PATH").unwrap_or_else(|_| "service_state.db".to_string());
    let client = ProverClient::from_env();
    // Create parent directory if it doesn't exist
    if let Some(parent) = Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).context("Failed to create database directory")?;
    }
    // Initialize the state manager with a database file
    let state_manager = StateManager::new(Path::new(&db_path))?;
    // Delete state if --delete flag is set
    if args.delete {
        state_manager.delete_state()?;
        println!("State file deleted successfully");
        return Ok(());
    }
    let state_manager = StateManager::new(Path::new(&db_path))?; // Load or initialize the service state
    let mut service_state = match state_manager.load_state()? {
        Some(state) => state,
        None => state_manager.initialize_state(7606080)?,
    };
    let elfs_path = std::env::var("ELFS_OUT").unwrap_or_else(|_| "elfs/variable".to_string());

    let recursive_elf_path = Path::new(&elfs_path).join("recursive-elf.bin");
    let wrapper_elf_path = Path::new(&elfs_path).join("wrapper-elf.bin");

    // Generate the Recursion Circuit
    if args.generate_recursion_circuit.is_some() {
        let initial_slot = args.generate_recursion_circuit.unwrap_or(7606080);
        // Initialize the preprocessor with the current trusted slot
        let preprocessor = Preprocessor::new(service_state.trusted_slot);
        // Get the next block's inputs for proof generation
        let inputs = preprocessor.run().await?;

        let helios_inputs: HeliosInputs = serde_cbor::from_slice(&inputs)?;
        let trusted_committee_hash = helios_inputs
            .store
            .current_sync_committee
            .clone()
            .tree_hash_root()
            .to_vec();

        let committee_hash_formatted = format!("{:?}", trusted_committee_hash);
        let template = include_str!("../../recursion/circuit/src/blueprint.rs");

        let generated_code = template
            .replace("{ committee_hash }", &committee_hash_formatted)
            .replace("{ trusted_head }", &initial_slot.to_string());
        write("recursion/circuit/src/main.rs", generated_code)
            .context("Failed to generate recursive circuit from blueprint")?;

        println!("Recursive circuit generated successfully");
        return Ok(());
    }

    // Generate the Wrapper Circuit
    if args.generate_wrapper_circuit {
        let (_, vk) = client.setup(RECURSIVE_ELF_RUNTIME);
        let vk_bytes = vk.bytes32();
        let template = include_str!("../../recursion/wrapper-circuit/src/blueprint.rs");
        let generated_code = template.replace("{ recursive_vk }", &format!("\"{}\"", vk_bytes));
        write("recursion/wrapper-circuit/src/main.rs", generated_code)
            .context("Failed to generate wrapper circuit from blueprint")?;
        println!("Wrapper circuit generated successfully");
        return Ok(());
    }

    // Dump the ELFs as bytes
    if args.dump_elfs {
        std::fs::create_dir_all(&elfs_path)?;

        // Create parent directory if it doesn't exist
        if let Some(parent) = Path::new(&elfs_path).parent() {
            std::fs::create_dir_all(parent).context("Failed to create ELF directory")?;
        }

        std::fs::write(&recursive_elf_path, RECURSIVE_ELF_RUNTIME).context(format!(
            "Failed to dump recursive ELF to {}",
            recursive_elf_path.display()
        ))?;
        std::fs::write(&wrapper_elf_path, WRAPPER_ELF_RUNTIME).context(format!(
            "Failed to dump wrapper ELF to {}",
            wrapper_elf_path.display()
        ))?;
        println!("ELFs dumped successfully");
        return Ok(());
    }

    if !recursive_elf_path.exists() {
        println!(
            "Recursive ELF not found at {}, please run with --dump-elfs",
            recursive_elf_path.display()
        );
        return Err(anyhow::anyhow!("Recursive ELF not found"));
    }

    // read bytes of recursive-elf and wrapper-elf
    let recursive_elf = std::fs::read(&recursive_elf_path).context(format!(
        "Failed to read recursive elf from {}",
        recursive_elf_path.display()
    ))?;
    let wrapper_elf = std::fs::read(&wrapper_elf_path).context(format!(
        "Failed to read wrapper elf from {}",
        wrapper_elf_path.display()
    ))?;

    // Main service loop
    loop {
        // Set up the proving keys and verification keys for all circuits
        let (helios_pk, helios_vk) = client.setup(HELIOS_ELF);
        let (recursive_pk, recursive_vk) = client.setup(&recursive_elf);
        let (wrapper_pk, wrapper_vk) = client.setup(&wrapper_elf);
        println!("Recursive VK: {:?}", recursive_vk.bytes32());
        // Initialize the preprocessor with the current trusted slot
        let preprocessor = Preprocessor::new(service_state.trusted_slot);

        // Get the next block's inputs for proof generation
        let inputs = match preprocessor.run().await {
            Ok(inputs) => inputs,
            Err(e) => {
                println!("[Warning]: {:?}", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                continue;
            }
        };
        let mut stdin = SP1Stdin::new();
        stdin.write_slice(&inputs);

        // For the first proof, store the genesis sync committee hash
        if service_state.update_counter == 0 {
            let helios_inputs: HeliosInputs = serde_cbor::from_slice(&inputs)?;
            service_state.genesis_committee_hash = Some(hex::encode(
                helios_inputs
                    .store
                    .current_sync_committee
                    .clone()
                    .tree_hash_root()
                    .to_vec(),
            ));
        }

        // Generate the Helios proof
        let proof = match client
            .prove(&helios_pk, &stdin)
            .groth16()
            .run()
            .context("Failed to prove")
        {
            Ok(proof) => proof,
            Err(e) => {
                println!("Proof failed with error: {:?}", e);
                continue;
            }
        };

        // Decode the Helios proof outputs
        let helios_outputs: HeliosOutputs =
            HeliosOutputs::abi_decode(&proof.public_values.to_vec(), false).unwrap();

        // Fetch additional block data needed for execution payload (state_root, height) verification
        let electra_block =
            get_electra_block(helios_outputs.newHead.try_into()?, &consensus_url).await;

        // Extract and process block data
        let electra_body_roots = extract_electra_block_body(electra_block);
        let beacon_header =
            get_beacon_block_header(helios_outputs.newHead.try_into()?, &consensus_url).await;

        // Construct the zk-friendly Electra block header
        let electra_header = ElectraBlockHeader {
            slot: beacon_header.slot.as_u64(),
            proposer_index: beacon_header.proposer_index,
            parent_root: beacon_header.parent_root.to_vec().try_into().unwrap(),
            state_root: beacon_header.state_root.to_vec().try_into().unwrap(),
            body_root: beacon_header.body_root.to_vec().try_into().unwrap(),
        };

        // Get the previous proof if this isn't the first update
        let previous_proof = if service_state.update_counter == 0 {
            None
        } else {
            Some(
                service_state
                    .most_recent_proof
                    .expect("Missing previous proof in state"),
            )
        };

        let recursion_inputs = RecursionCircuitInputs {
            active_committee_hash: active_committee_hash,
            electra_body_roots: electra_body_roots,
            electra_header: electra_header,
            helios_proof: proof.bytes(),
            helios_public_values: proof.public_values.to_vec(),
            recursive_proof: previous_proof.as_ref().map(|p| p.bytes()),
            recursive_public_values: previous_proof.as_ref().map(|p| p.public_values.to_vec()),
            recursive_vk: previous_proof.as_ref().map(|_| wrapper_vk.bytes32()),
            previous_head: service_state.trusted_slot,
        };

        // Generate the recursive proof
        let mut stdin = SP1Stdin::new();
        stdin.write_slice(&borsh::to_vec(&recursion_inputs).unwrap());

        let recursive_proof = client
            .prove(&recursive_pk, &stdin)
            .groth16()
            .run()
            .context("Failed to prove")?;

        let wrapper_inputs = WrapperCircuitInputs {
            recursive_proof: recursive_proof.bytes(),
            recursive_public_values: recursive_proof.public_values.to_vec(),
            recursive_vk: wrapper_vk.bytes32(),
        };

        // Generate the recursive proof
        let mut stdin = SP1Stdin::new();
        stdin.write_slice(&borsh::to_vec(&wrapper_inputs).unwrap());

        // the final wrapped proof to send to the coprocessor
        let _final_wrapped_proof = client
            .prove(&wrapper_pk, &stdin)
            .groth16()
            .run()
            .context("Failed to prove")?;

        // Decode the recursive proof outputs
        let wrapped_outputs: RecursionCircuitOutputs =
            borsh::from_slice(&recursive_proof.public_values.to_vec()).unwrap();

        // Update the service state with new trusted information
        service_state.most_recent_proof = Some(recursive_proof.clone());
        service_state.trusted_slot = helios_outputs.newHead.try_into().unwrap();
        service_state.trusted_height = wrapped_outputs.height;
        service_state.trusted_root = wrapped_outputs.root.try_into().unwrap();
        service_state.update_counter += 1;
        // Save the updated state to the database
        state_manager.save_state(&service_state)?;

        // Log the updated state and elapsed time
        println!("New Service State: {:?} \n", service_state);
        let elapsed_time = start_time.elapsed();
        println!("Alive for: {:?}", elapsed_time);
    }
}
