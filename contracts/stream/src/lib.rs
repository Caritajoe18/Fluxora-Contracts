#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    InvalidParams = 1,
    StreamNotFound = 2,
    NotAuthorized = 3,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Config {
    pub token: Address,
    pub admin: Address,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamStatus {
    Active = 0,
    Paused = 1,
    Completed = 2,
    Cancelled = 3,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Stream {
    pub stream_id: u64,
    pub sender: Address,
    pub recipient: Address,
    pub deposit_amount: i128,
    pub rate_per_second: i128,
    pub start_time: u64,
    pub cliff_time: u64,
    pub end_time: u64,
    pub withdrawn_amount: i128,
    pub status: StreamStatus,
}

#[contracttype]
pub enum DataKey {
    Config,
    NextStreamId,
    Stream(u64),
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

fn get_config(env: &Env) -> Config {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .expect("contract not initialised")
}

fn get_token(env: &Env) -> Address {
    get_config(env).token
}

fn get_admin(env: &Env) -> Address {
    get_config(env).admin
}

fn get_stream_count(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::NextStreamId)
        .unwrap_or(0u64)
}

fn set_stream_count(env: &Env, count: u64) {
    env.storage().instance().set(&DataKey::NextStreamId, &count);
}

fn load_stream(env: &Env, stream_id: u64) -> Stream {
    env.storage()
        .persistent()
        .get(&DataKey::Stream(stream_id))
        .expect("stream not found")
}

fn save_stream(env: &Env, stream: &Stream) {
    let key = DataKey::Stream(stream.stream_id);
    env.storage().persistent().set(&key, stream);
    env.storage().persistent().extend_ttl(&key, 17280, 120960);
}

// ---------------------------------------------------------------------------
// Contract Implementation
// ---------------------------------------------------------------------------

#[contract]
pub struct FluxoraStream;

#[contractimpl]
impl FluxoraStream {
    pub fn init(env: Env, token: Address, admin: Address) {
        if env.storage().instance().has(&DataKey::Config) {
            panic!("already initialised");
        }
        let config = Config { token, admin };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::NextStreamId, &0u64);
        env.storage().instance().extend_ttl(17280, 120960);
    }

    pub fn create_stream(
        env: Env,
        sender: Address,
        recipient: Address,
        deposit_amount: i128,
        rate_per_second: i128,
        start_time: u64,
        cliff_time: u64,
        end_time: u64,
    ) -> u64 {
        sender.require_auth();

        if deposit_amount <= 0 {
            panic!("deposit_amount must be positive");
        }
        if rate_per_second <= 0 {
            panic!("rate_per_second must be positive");
        }
        if sender == recipient {
            panic!("sender and recipient must be different");
        }
        if end_time <= start_time {
            panic!("end_time must be greater than start_time");
        }
        if cliff_time < start_time || cliff_time > end_time {
            panic!("cliff_time must be within [start_time, end_time]");
        }

        let duration = (end_time - start_time) as i128;
        let total_streamable = rate_per_second.checked_mul(duration).expect("overflow");
        if deposit_amount < total_streamable {
            panic!("deposit_amount must cover total streamable amount");
        }

        let token_client = token::Client::new(&env, &get_token(&env));
        token_client.transfer(&sender, &env.current_contract_address(), &deposit_amount);

        let stream_id = get_stream_count(&env);
        set_stream_count(&env, stream_id + 1);

        let stream = Stream {
            stream_id,
            sender,
            recipient,
            deposit_amount,
            rate_per_second,
            start_time,
            cliff_time,
            end_time,
            withdrawn_amount: 0,
            status: StreamStatus::Active,
        };

        save_stream(&env, &stream);
        env.events()
            .publish((symbol_short!("created"), stream_id), deposit_amount);

        stream_id
    }

    pub fn pause_stream(env: Env, stream_id: u64) {
        let mut stream = load_stream(&env, stream_id);
        Self::require_sender_or_admin(&env, &stream.sender);

        if stream.status != StreamStatus::Active {
            panic!("stream is not active");
        }
        stream.status = StreamStatus::Paused;
        save_stream(&env, &stream);

        env.events()
            .publish((symbol_short!("paused"), stream_id), ());
    }

    pub fn resume_stream(env: Env, stream_id: u64) {
        let mut stream = load_stream(&env, stream_id);
        Self::require_sender_or_admin(&env, &stream.sender);

        if stream.status != StreamStatus::Paused {
            panic!("stream is not paused");
        }
        stream.status = StreamStatus::Active;
        save_stream(&env, &stream);

        env.events()
            .publish((symbol_short!("resumed"), stream_id), ());
    }

    pub fn cancel_stream(env: Env, stream_id: u64) {
        let mut stream = load_stream(&env, stream_id);
        Self::require_sender_or_admin(&env, &stream.sender);

        if stream.status != StreamStatus::Active && stream.status != StreamStatus::Paused {
            panic!("stream must be active or paused to cancel");
        }

        let accrued = Self::calculate_accrued(env.clone(), stream_id);
        let unstreamed = stream.deposit_amount - accrued;

        if unstreamed > 0 {
            let token_client = token::Client::new(&env, &get_token(&env));
            token_client.transfer(&env.current_contract_address(), &stream.sender, &unstreamed);
        }

        stream.status = StreamStatus::Cancelled;
        save_stream(&env, &stream);

        env.events()
            .publish((symbol_short!("cancelled"), stream_id), unstreamed);
    }

    pub fn withdraw(env: Env, stream_id: u64) -> i128 {
        let mut stream = load_stream(&env, stream_id);
        stream.recipient.require_auth();

        if stream.status == StreamStatus::Completed {
            panic!("already completed");
        }
        if stream.status == StreamStatus::Paused {
            panic!("cannot withdraw from paused stream");
        }

        let accrued = Self::calculate_accrued(env.clone(), stream_id);
        let withdrawable = accrued - stream.withdrawn_amount;

        if withdrawable <= 0 {
            panic!("nothing to withdraw");
        }

        let token_client = token::Client::new(&env, &get_token(&env));
        token_client.transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &withdrawable,
        );

        stream.withdrawn_amount += withdrawable;

        if env.ledger().timestamp() >= stream.end_time
            && stream.withdrawn_amount >= stream.deposit_amount
        {
            stream.status = StreamStatus::Completed;
        }

        save_stream(&env, &stream);
        env.events()
            .publish((symbol_short!("withdrew"), stream_id), withdrawable);

        withdrawable
    }

    pub fn calculate_accrued(env: Env, stream_id: u64) -> i128 {
        let stream = load_stream(&env, stream_id);
        let now = env.ledger().timestamp();

        if now < stream.cliff_time {
            return 0;
        }

        let elapsed = (now.min(stream.end_time)).saturating_sub(stream.start_time) as i128;
        let accrued = elapsed * stream.rate_per_second;

        accrued.min(stream.deposit_amount)
    }

    pub fn get_config(env: Env) -> Config {
        get_config(&env)
    }

    pub fn get_stream_state(env: Env, stream_id: u64) -> Stream {
        load_stream(&env, stream_id)
    }

    fn require_sender_or_admin(env: &Env, sender: &Address) {
        let admin = get_admin(env);
        if sender != &admin {
            sender.require_auth();
        } else {
            admin.require_auth();
        }
    }
}

#[contractimpl]
impl FluxoraStream {
    pub fn cancel_stream_as_admin(env: Env, stream_id: u64) {
        get_admin(&env).require_auth();
        Self::cancel_stream(env, stream_id);
    }
}

#[cfg(test)]
mod test;
