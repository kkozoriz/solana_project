use crate::config;
use crate::errors::Result;
use borsh::BorshDeserialize;
use common::Counter;
use common::CounterInstruction;
use solana_client::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use solana_sdk::signer::keypair::write_keypair_file;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;
use std::path::Path;
use std::str::FromStr;

static COUNTER_ACCOUNT_SEED: &str = "COUNTER";
static COUNTER_ACCOUNT_DATA_SIZE: usize = std::mem::size_of::<Counter>();
//bytes
static PROGRAM_PATH: &str = "/Users/sliwmen/RustroverProjects/solana_project/target/deploy/program.so";
static PROGRAM_KEYPAIR: &str = "/Users/sliwmen/RustroverProjects/solana_project/target/deploy/program-keypair.json";

pub struct Client {
    client: RpcClient,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            client: Self::get_rpc_client(),
        }
    }

    //Get a handle to rpc client for connecting to solana network
    fn get_rpc_client() -> RpcClient {
        let network = match config::get_config("json_rpc_url") {
            Some(network) => network,
            None => String::from("http://localhost:8899"),
        };
        let commitment = match config::get_config("commitment") {
            Some(commitment) => commitment,
            None => String::from("confirmed"),
        };

        //Falls back on 'confirmed' commitment level
        let commitment = CommitmentConfig::from_str(&commitment)
            .unwrap_or_else(|_| CommitmentConfig::confirmed());

        println!("Connecting to cluster...{}", network);
        let client = RpcClient::new_with_commitment(network, commitment);
        let version = client
            .get_version()
            .expect("Error getting node solana version");
        println!("Connection to cluster established");
        println!("Cluster node solana version {:?}", version);
        client
    }

    //Get the keypair from which all transaction fees are paid
    //Or generate the keypair if it does not exist
    pub fn get_payer_keypair() -> Option<Keypair> {
        let keypair_path = match config::get_config("keypair_path") {
            Some(keypair_path) => keypair_path,
            None => {
                eprintln!("Keypair path not found in ~/.config/solana/cli/config.yaml");
                let mut home_dir = home::home_dir()?;
                home_dir.push(".config/solana/id.json");
                home_dir.into_os_string().into_string().ok()?
            }
        };

        let payer = match config::get_keypair(&keypair_path) {
            Some(keypair) => keypair,
            None => {
                println!("Generating new payer keypair...");
                let keypair = Keypair::new();
                let _ignored = write_keypair_file(&keypair, &keypair_path);
                assert!(Path::new(&keypair_path).exists());
                keypair
            }
        };
        Some(payer)
    }

    //Get the payer account balance from the network
    pub fn get_payer_account_balance(&self) -> Result<u64> {
        let payer = Self::get_payer_keypair().ok_or("Error getting payer keypair")?;
        self.client
            .get_balance(&payer.pubkey())
            .map_err(|err| format!("Error getting payer balance {}", err))
    }

    //Get the program public key
    //First look at the environment variable 'program_id' falling back on PROGRAM_KEYPAIR
    //Pass in the program_id env variable if the program was deployed with command
    //'solana depoly program.so[path to .so] instead of 'solana program deploy program.so'
    pub fn get_program_id() -> Option<Pubkey> {
        match std::env::var("program_id") {
            Ok(ref program_id) => Pubkey::from_str(program_id).ok(),
            Err(_err) => config::get_keypair(PROGRAM_KEYPAIR).map(|keypair| keypair.pubkey()),
        }
    }

    //Derive the address (public key) of a counter account from payer, seed
    //and program id
    pub fn get_counter_pubkey() -> Pubkey {
        let payer_seed_prog = Self::get_payer_keypair()
            .map(|keypair| (keypair.pubkey(), COUNTER_ACCOUNT_SEED))
            .and_then(|(payer_pubkey, seed)| {
                Self::get_program_id().map(|program_id| (payer_pubkey, seed, program_id))
            });
        match payer_seed_prog {
            Some((ref payer_pubkey, seed, ref program_id)) => {
                Pubkey::create_with_seed(payer_pubkey, seed, program_id)
                    .expect("Error creating counter account key")
            }
            None => panic!("Error creating account key"),
        }
    }

    //Request airdrop for executing transaction if account balance is not sufficient
    //To skip airdrop(for experimentation) pass in the environment variable skip_airdrop to
    //some value
    pub fn request_airdrop(&self, txn_amt: u64) -> Result<()> {
        if std::env::var("skip_airdrop").ok().is_some() {
            return Ok(());
        }
        let balance = self.get_payer_account_balance()?;
        if balance < txn_amt {
            let payer = Self::get_payer_keypair().ok_or("Payer keypair not found")?;
            let payer_pubkey = payer.pubkey();
            println!(
                "Account balance {} is not sufficient for transaction. Requesting airdrop for
          {} lamports",
                balance,
                txn_amt - balance
            );
            let sig = self
                .client
                .request_airdrop(&payer_pubkey, txn_amt - balance)
                .map_err(|err| format!("Airdrop for payer failed {}", err))?;
            //Wait a while for airdrop transaction to confirm
            while !self
                .client
                .confirm_transaction(&sig)
                .map_err(|err| format!("Airdrop confirmation failed {}", err))?
            {}
        }
        Ok(())
    }

    /**
     * For solana program to store state - we need an additional account since solana
     * program is immutable
     * Setup the counter account if it does not exist. Checks the amount needed to fund
     * the greeting account based on its size. Also takes into account the cost of invoking
     * the create account transaction
     */

    pub fn setup_counter_account(&self) -> Result<()> {
        let payer = Self::get_payer_keypair().ok_or("Payer keypair not found")?;
        let payer_pubkey = payer.pubkey();
        let program_id = Self::get_program_id()
            .ok_or("Program pubkey not found! Program may not have been built")?;

        let counter_pubkey = Self::get_counter_pubkey();
        //Check if the counter account has already been created, if not create one
        let account = self.client.get_account(&counter_pubkey);
        match account {
            Ok(account) => {
                println!(
                    "Counter account {} already exists. Owner program: {:?}",
                    counter_pubkey, account.owner
                );
                Ok(())
            }
            Err(err) => {
                eprintln!("Counter account does not exist {}. Would create", err);
                let freestay_lamports = self
                    .client
                    .get_minimum_balance_for_rent_exemption(COUNTER_ACCOUNT_DATA_SIZE)
                    .map_err(|err| {
                        format!("Error getting Minimum balance for rent exemption {}", err)
                    })?;
                println!("Freestay lamports : {}", freestay_lamports);
                let instruction = system_instruction::create_account_with_seed(
                    &payer_pubkey,                    //from_keypair
                    &counter_pubkey,                  //to_keypair
                    &payer_pubkey,                    //base
                    COUNTER_ACCOUNT_SEED,             //seed
                    freestay_lamports,                //lamports
                    COUNTER_ACCOUNT_DATA_SIZE as u64, //space
                    &program_id,                      //owner
                );
                //Query latest block hash
                let blockhash = self
                    .client
                    .get_latest_blockhash()
                    .map_err(|s| format!("Error retrieving Latest block hash {}", s))?;

                let message =
                    Message::new_with_blockhash(&[instruction], Some(&payer_pubkey), &blockhash);
                //Check lamports needed to send this message
                let fee_for_message = self
                    .client
                    .get_fee_for_message(&message)
                    .map_err(|_| "Error getting fee for message")?;
                println!("Fee for message {}", fee_for_message);
                let total_amt = fee_for_message + freestay_lamports;
                println!("Total amount for transaction {} lamports", total_amt);
                //Request airdrop if needed
                self.request_airdrop(total_amt).map_err(|s| {
                    format!("Airdrop failed while setting up counter account {}", s)
                })?;

                let transaction = Transaction::new(&[&payer], message, blockhash);
                let _signature = self
                    .client
                    .send_and_confirm_transaction(&transaction)
                    .map_err(|err| format!("Error sending account setup transaction {}", err))?;
                Ok(())
            }
        }
    }

    //Check if the program has been deployed
    //This program expects that the on-chain program be deployed as `solana program deploy
    //program.so` - this ensures that the deployed program is owned by upgradeable_loader
    //and the deployed program's byte codes are stored in a separate account(progradata
    //_address) owned by the upgradeable bpf loader.
    //On the other hand, if the program is deployed as 'solana deploy program.so[path to .so],
    //the deployed program is owned by bpf loader and a random program id is generated and
    //program byte codes are stored in the program account itself.
    //We are expecting a predictable program pubkey from the keypair generated when we use
    //'solana program deploy program.so [path to program .so file] to deploy our program
    //In case, we deploy our program via the command 'solana deploy program.so' - we expect
    //program pubkey(which is random pubkey generated by above deployment command) to be
    //passed in as 'program_id' environment parameter
    //
    //Though it is advisable to use `solana program deploy  program.so`, someone could also do a
    //Note - programs deployed to upgradable loader can be closed('solana program close
    //program_id] - which wipes out program byte codes from program data account - but
    //program account 'executable' flag still returns true - that is the reason we are
    //we are looking at programdata account to make sure the program has not been closed

    pub fn check_program(&self) -> Result<()> {
        let program_id = Self::get_program_id()
            .ok_or("Program pubkey not found! Program may not have been built")?;
        let _ = match self.client.get_account(&program_id) {
            Ok(program_account) => {
                let owner = program_account.owner.to_string();
                let upgradeable_loader_id = solana_sdk::bpf_loader_upgradeable::id();
                let upgradeable_loader_id_str = upgradeable_loader_id.to_string();
                let bpf_loader_id = solana_sdk::bpf_loader::id().to_string();

                match owner {
                    //Upgradable bpf loader owned account return true even if the
                    //program may have been closed. Hence we are checking programdata
                    //account make sure the program has not been closed
                    ref s if s == &upgradeable_loader_id_str => {
                        let programdata_address = Pubkey::try_find_program_address(
                            &[program_id.as_ref()],
                            &upgradeable_loader_id,
                        );
                        match programdata_address {
                            Some((programdata_address, _seed)) => {
                                //We are good if we can retrieve the account - otherwise
                                //close call would have emptied the account
                                let program_binary_acc =
                                    self.client.get_account(&programdata_address);
                                match program_binary_acc {
                                    Ok(_) => {
                                        println!("Binary address {}", programdata_address);
                                        Ok(())
                                    }
                                    Err(err) => {
                                        eprintln!("Could not find program binary {}", err);
                                        Err("Could not find program binary")
                                    }
                                }
                            }
                            None => Err("Failed to get program data account address"),
                        }
                    }
                    //Bpf loader owned accounts are immutable and they store program
                    //byte code in the program account itself - executable flag is always
                    //true
                    ref s if s == &bpf_loader_id && program_account.executable => Ok(()),
                    //This check is actually redundant
                    ref s if s == &bpf_loader_id && !program_account.executable => {
                        panic!("Not executable!")
                    }
                    _ => panic!("This whould never happen!"),
                }
            }
            Err(err) => {
                eprintln!("Error retrieving on-chain program account info {}", err);
                match Path::new(PROGRAM_PATH).exists() {
                    true => return Err("On-chain program may not have been deployed".to_string()),
                    false => return Err("On-chain program may not have been built".to_string()),
                }
            }
        };

        Ok(())
    }

    //Send a transaction to increament the counter

    pub fn increament_counter(&self) -> Result<()> {
        let payer = Self::get_payer_keypair().ok_or("Payer keypair not found")?;
        let payer_pubkey = payer.pubkey();
        let program_id = Self::get_program_id()
            .ok_or("Program pubkey not found! Program may not have been built")?;
        let counter_pubkey = Self::get_counter_pubkey();

        let counter_instruction = CounterInstruction::Increament;
        let instruction = Instruction::new_with_borsh(
            program_id,
            &counter_instruction,
            vec![AccountMeta::new(counter_pubkey, false)],
        );

        let blockhash = self
            .client
            .get_latest_blockhash()
            .map_err(|err| format!("Latest block hash {}", err))?;
        let message = Message::new_with_blockhash(&[instruction], Some(&payer_pubkey), &blockhash);
        //let message = Message::new_with_blockhash(&[instruction.clone(), instruction.clone()], Some(&payer_pubkey), &blockhash);
        //Check lamports needed to execute this message
        let fee_for_message = self
            .client
            .get_fee_for_message(&message)
            .map_err(|err| format!("Failed getting fee for message {}", err))?;
        println!("Fee for message {}", fee_for_message);
        //Request airdrop if needed
        self.request_airdrop(fee_for_message)
            .map_err(|s| format!("Error during airdrop {}", s))?;
        let transaction = Transaction::new(&[&payer], message, blockhash);

        let _signature = self
            .client
            .send_and_confirm_transaction(&transaction)
            .map_err(|err| format!("Error while sending counter increament transaction {}", err))?;
        Ok(())
    }

    //Get the increamented counter value
    pub fn get_counter_reading(&self) -> Result<()> {
        let program_id = Self::get_program_id()
            .ok_or("Program pubkey not found! Program may not have been built")?;
        //Retrieve all accounts owned by our program
        let counter_accounts = self
            .client
            .get_program_accounts(&program_id)
            .map_err(|err| format!("Program counter account may not have been setup {}", err))?;
        //We have only one account that is owned by our program
        let counter_account: &Account = match counter_accounts.first() {
            //We are ignoring the first element of the returned tuple
            Some((_pubkey, account)) => account,
            None => return Err("Counter account not found".to_string()),
        };
        //Get the data field out of the account
        let data = &counter_account.data;
        //Deserialize it back to a Counter
        let counter = Counter::try_from_slice(data)
            .map_err(|err| format!("Error deserializing bytes to counter {}", err))?;
        println!("Counter value {}", counter.count);
        Ok(())
    }
}