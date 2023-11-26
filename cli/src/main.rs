use anyhow::{anyhow, Context, Error};
use bfsp::auth::{Authentication, CreateUserRequest, LoginRequest};
use bfsp::cli::{compressed_encrypted_chunk_from_file, use_parallel_hasher, FileHeader};
use bfsp::file_server_message::{
    ChunksUploadedQuery, DownloadChunkQuery, Message, UploadChunkQuery,
};
use bfsp::{download_chunk_resp, ChunkHash, Message as MessageTrait};
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Display;
use std::path::Path;

use anyhow::Result;
use bfsp::{
    hash_chunk, hash_file, parallel_hash_chunk, ChunkID, ChunkMetadata, ChunksUploadedQueryResp,
    DownloadChunkResp, EncryptionKey, FileHash, FileServerMessage, UploadChunkResp,
};
use log::{debug, trace};
use tokio::fs::{self, canonicalize, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Upload {
        file_path: String,
    },
    Download {
        file_path: String,
        download_to: Option<String>,
    },
    Signup {
        email: String,
        password: String,
    },
    Login {
        email: String,
        password: String,
    },
}

#[tokio::main]
async fn main() {
    fern::Dispatch::new()
        .format(|out, msg, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                humantime::format_rfc3339(std::time::SystemTime::now()),
                record.level(),
                record.target(),
                msg
            ))
        }) // Add blanket level filter -
        .level(log::LevelFilter::Trace)
        .level_for("sqlx", log::LevelFilter::Warn)
        .level_for("hyper", log::LevelFilter::Warn)
        // - and per-module overrides
        // Output to stdout, files, and other Dispatch configurations
        .chain(std::io::stdout())
        .chain(fern::log_file("output.log").unwrap())
        // Apply globally
        .apply()
        .unwrap();

    let pool_url = |_| {
        format!(
            "sqlite:{}/data.db",
            env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string())
        )
    };
    let pool = sqlx::SqlitePool::connect(&env::var("DATABASE_URL").unwrap_or_else(pool_url))
        .await
        .unwrap();

    sqlx::migrate!().run(&pool).await.unwrap();

    let client = reqwest::ClientBuilder::new().build().unwrap();

    let args = Args::parse();

    match args.command {
        Commands::Upload { file_path } => {
            let (key, macaroon): (EncryptionKey, String) =
                match sqlx::query("select enc_key, macaroon from keys")
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
                {
                    Some(key_info) => (
                        key_info.get::<Vec<u8>, _>("enc_key").try_into().unwrap(),
                        key_info.get::<String, _>("macaroon"),
                    ),
                    None => {
                        println!("Please login.");
                        return;
                    }
                };

            let mut sock = TcpStream::connect("127.0.0.1:9999").await.unwrap();

            if let Err(err) = add_file(&file_path, &pool).await {
                debug!("Error adding file: {err:?}");

                if let Some(err) = err.downcast_ref::<AddFileRecoverableErr>() {
                    match err {
                        AddFileRecoverableErr::FileAlreadyAdded => (),
                        AddFileRecoverableErr::ChunkAlreadyAdded => (),
                    }
                } else {
                    panic!("{err}");
                }
            }

            let file_headers = file_headers_from_path(&file_path, &pool).await.unwrap();
            let file_header = file_headers.first().unwrap();

            upload_file(&file_path, file_header, &mut sock, &key, macaroon)
                .await
                .unwrap()
        }
        Commands::Download {
            file_path,
            download_to,
        } => {
            let (key, macaroon): (EncryptionKey, String) =
                match sqlx::query("select enc_key, macaroon from keys")
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
                {
                    Some(key_info) => (
                        key_info.get::<Vec<u8>, _>("enc_key").try_into().unwrap(),
                        key_info.get::<String, _>("macaroon"),
                    ),
                    None => {
                        println!("Please login.");
                        return;
                    }
                };

            let mut sock = TcpStream::connect("127.0.0.1:9999").await.unwrap();
            let file_headers = file_headers_from_path(&file_path, &pool).await.unwrap();
            let file_header = file_headers.first().ok_or("File not found").unwrap();

            download_file(
                &file_header,
                download_to.unwrap_or_else(|| "./".to_string()),
                &mut sock,
                &key,
                macaroon,
            )
            .await
            .unwrap();
        }
        Commands::Signup { email, password } => {
            let response = client
                .post("http://127.0.0.1:3000/create_user")
                .json(&CreateUserRequest { email, password })
                .send()
                .await
                .unwrap();

            let response_status = response.status();

            if response_status.is_success() {
                println!("Successfully registered! Please sign in");
            } else {
                let response_text = response.text().await.unwrap();
                println!("Error when trying to register: '{}'", response_text);
            }
        }
        Commands::Login { email, password } => {
            let response = client
                .post("http://127.0.0.1:3000/login_user")
                .json(&LoginRequest { email, password })
                .send()
                .await
                .unwrap();

            let response_status = response.status();
            let response_text = response.text().await.unwrap();

            if response_status.is_success() {
                let key = EncryptionKey::new();
                sqlx::query("insert into keys (enc_key, username, macaroon) values ( ?, ?, ? )")
                    .bind(key)
                    .bind("billy")
                    .bind(response_text)
                    .execute(&pool)
                    .await
                    .unwrap();

                println!("Successfully logged in!!!")
            } else {
                println!("Got response: '{response_text}' with status: '{response_status}'");
            }
        }
    }
}

pub async fn query_chunks_uploaded(
    file_hash: &FileHash,
    sock: &mut TcpStream,
    pool: &SqlitePool,
    macaroon: String,
) -> Result<ChunksUploadedQueryResp> {
    trace!("Querying chunks uploaded");

    let chunk_ids = sqlx::query("select id from chunks where file_hash = ?")
        .bind(file_hash)
        .fetch_all(pool)
        .await
        .with_context(|| format!("Failed to get chunk IDs for file hash {}", file_hash))?
        .into_iter()
        .map(|chunk_info| chunk_info.get::<String, _>("id").try_into())
        .collect::<Result<HashSet<ChunkID>>>()?;

    let msg = FileServerMessage {
        auth: Some(Authentication { macaroon }),
        message: Some(Message::ChunksUploadedQuery(ChunksUploadedQuery {
            chunk_ids: chunk_ids
                .into_iter()
                .map(|id| id.to_bytes().to_vec())
                .collect(),
        })),
    };

    sock.write_all(msg.to_bytes().as_slice()).await?;

    let resp_len = sock.read_u32_le().await?;
    let mut resp_bytes = vec![0; resp_len as usize];
    sock.read_exact(&mut resp_bytes).await?;

    let resp = ChunksUploadedQueryResp::decode(resp_bytes.as_slice())
        .map_err(|_| anyhow!("Error deserializing ChunksUploadedQueryResp"))?;

    trace!("Finished querying chunks uploaded");
    Ok(resp)
}

#[derive(Debug)]
pub enum AddFileRecoverableErr {
    FileAlreadyAdded,
    ChunkAlreadyAdded,
}

impl Display for AddFileRecoverableErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AddFileRecoverableErr::FileAlreadyAdded => "File already added",
            AddFileRecoverableErr::ChunkAlreadyAdded => "Chunk already added",
        })
    }
}

impl std::error::Error for AddFileRecoverableErr {}

// TODO: delete unused chunks on server
pub async fn update_file(file_path: &str, file: &mut File, pool: &SqlitePool) -> Result<()> {
    trace!("Updating file {file_path}");

    let file_path = fs::canonicalize(file_path).await?;
    let file_path = file_path.to_str().unwrap();

    let original_file_hash: FileHash = sqlx::query("select hash from files where file_path = ?")
        .bind(file_path)
        .fetch_one(pool)
        .await
        .with_context(|| "Error getting file id for updating file")?
        .get::<String, &str>("hash")
        .try_into()?;

    let file_header = FileHeader::from_file(file).await?;
    file.rewind().await?;
    let file_hash = hash_file(file).await?;
    file.rewind().await?;

    let num_chunks: i64 = file_header.chunks.len().try_into().unwrap();

    sqlx::query("update files set hash = ?, chunk_size = ?, chunks = ? where file_path = ?")
        .bind(file_hash)
        .bind(file_header.chunk_size)
        .bind(num_chunks)
        .bind(file_path)
        .execute(pool)
        .await
        .map_err(|err| match err {
            sqlx::Error::Database(err) => match err.kind() {
                sqlx::error::ErrorKind::UniqueViolation => {
                    Error::new(AddFileRecoverableErr::FileAlreadyAdded)
                }
                _ => anyhow!("Unknown database error: {err:#}"),
            },
            _ => anyhow!("Unknown database error: {err:#}"),
        })?;

    trace!("Deleting chunks for file {file_path}");

    sqlx::query("delete from chunks where file_hash = ?")
        .bind(original_file_hash)
        .execute(pool)
        .await?;

    trace!("Inserting chunks for file {file_path}");

    for (chunk_id, chunk) in file_header.chunks.iter() {
        sqlx::query(
            r#"insert into chunks
                (hash, id, file_hash, chunk_size)
                values ( ?, ?, ?, ? )
            "#,
        )
        .bind(&chunk.hash)
        .bind(chunk_id)
        .bind(&file_header.hash)
        .bind(chunk.size)
        .execute(pool)
        .await?;
    }

    Ok(())
}

pub async fn add_file(file_path: &str, pool: &SqlitePool) -> Result<()> {
    trace!("Adding file {file_path}");

    let file_path = fs::canonicalize(file_path).await?;
    let file_path = file_path.to_str().unwrap();

    let mut file = fs::File::open(file_path)
        .await
        .with_context(|| format!("Failed to read file {file_path} for adding"))?;
    let file_header = FileHeader::from_file(&mut file).await?;

    file.rewind().await?;

    let file_hash = hash_file(&mut file).await?;
    file.rewind().await?;

    let num_chunks: i64 = file_header.chunks.len().try_into().unwrap();

    sqlx::query(
        r#"insert into files
            (file_path, hash, chunk_size, chunks)
            values ( ?, ?, ?, ? )
    "#,
    )
    .bind(file_path)
    .bind(&file_hash)
    .bind(file_header.chunk_size)
    .bind(num_chunks)
    .execute(pool)
    .await
    .map_err(|err| match err {
        sqlx::Error::Database(err) => match err.kind() {
            sqlx::error::ErrorKind::UniqueViolation => {
                Error::new(AddFileRecoverableErr::FileAlreadyAdded)
            }
            _ => anyhow!("Unknown database error: {err:#}"),
        },
        _ => anyhow!("Unknown database error: {err:#}"),
    })?;

    trace!("Inserting chunks");

    //TODO: batch job this
    for (chunk_id, chunk) in file_header.chunks.iter() {
        let indice: i64 = (*file_header.chunk_indices.get(&chunk_id).unwrap())
            .try_into()
            .unwrap();

        sqlx::query(
            r#"insert into chunks
                (hash, id, chunk_size, indice, file_hash, nonce )
                values ( ?, ?, ?, ?, ?, ? )
            "#,
        )
        .bind(ChunkHash::from_bytes(chunk.clone().hash.try_into().unwrap()).to_string())
        .bind(chunk_id)
        .bind(chunk.size)
        .bind(indice)
        .bind(&file_hash)
        .bind(&chunk.nonce)
        .execute(pool)
        .await
        .map_err(|err| match err {
            sqlx::Error::Database(err) => match err.kind() {
                sqlx::error::ErrorKind::UniqueViolation => {
                    Error::new(AddFileRecoverableErr::ChunkAlreadyAdded)
                }
                _ => anyhow!("Unknown database error: {err:#}"),
            },
            _ => anyhow!("Unknown database error: {err:#}"),
        })?;
    }

    Ok(())
}

pub async fn upload_file(
    file_path: &str,
    file_header: &FileHeader,
    sock: &mut TcpStream,
    key: &EncryptionKey,
    macaroon: String,
) -> Result<()> {
    trace!("Uploading file");

    // TODO: optimize with query_chunks_uploaded
    let chunks_to_upload = file_header.chunks.values();

    trace!("Uploading {} chunks", chunks_to_upload.len());
    let mut file = OpenOptions::new()
        .read(true)
        .open(file_path)
        .await
        .with_context(|| "Reading file for upload")?;

    for chunk_meta in chunks_to_upload.into_iter() {
        let chunk_id = ChunkID::from_bytes(chunk_meta.clone().id.try_into().unwrap());
        trace!("Uploading chunk {chunk_id}");

        let chunk =
            compressed_encrypted_chunk_from_file(file_header, &mut file, chunk_id, key).await?;

        let msg = FileServerMessage {
            auth: Some(Authentication {
                macaroon: macaroon.clone(),
            }),
            message: Some(Message::UploadChunkQuery(UploadChunkQuery {
                chunk_metadata: Some(chunk_meta.clone()),
                chunk,
            })),
        };

        trace!("Writing to socket");
        sock.write_all(msg.to_bytes().as_slice()).await?;

        trace!("Reading response");
        let resp_len = sock.read_u32_le().await?;

        let mut resp_bytes = vec![0; resp_len.try_into().unwrap()];
        sock.read_exact(&mut resp_bytes).await?;

        let resp = UploadChunkResp::decode(resp_bytes.as_slice())
            .map_err(|_| anyhow!("Error deserializing UploadChunkResp"))?;
    }

    trace!("Uploaded file");

    Ok(())
}

pub async fn download_file<P: AsRef<Path> + Display>(
    file_header: &FileHeader,
    path_to_download_to: P,
    sock: &mut TcpStream,
    key: &EncryptionKey,
    macaroon: String,
) -> Result<()> {
    trace!("Downloading file {path_to_download_to}");

    trace!("Creating or opening file {path_to_download_to}");

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path_to_download_to)
        .await?;

    trace!("Opened file {path_to_download_to}");

    let use_parallel_hasher = use_parallel_hasher(file_header.chunk_size as usize);
    let hash_chunk_fn = match use_parallel_hasher {
        true => parallel_hash_chunk,
        false => hash_chunk,
    };

    debug!("File header chunks len: {}", file_header.chunks.len());

    // TODO: can this be optimized by seing which chunks have changed
    for chunk_id in file_header.chunks.keys() {
        trace!("Requesting chunk {chunk_id}");

        let msg = FileServerMessage {
            auth: Some(Authentication {
                macaroon: macaroon.clone(),
            }),
            message: Some(Message::DownloadChunkQuery(DownloadChunkQuery {
                chunk_id: chunk_id.to_bytes().to_vec(),
            })),
        };

        sock.write_all(msg.to_bytes().as_slice()).await?;

        let resp_len = sock.read_u32_le().await?;
        let mut resp_bytes = vec![0; resp_len.try_into().unwrap()];
        sock.read_exact(&mut resp_bytes).await?;

        let resp = DownloadChunkResp::decode(resp_bytes.as_slice())?;
        let (chunk_metadata, mut chunk_buf): (ChunkMetadata, Vec<u8>) = match resp.response.unwrap()
        {
            download_chunk_resp::Response::ChunkData(chunk_data) => {
                (chunk_data.chunk_metadata.unwrap(), chunk_data.chunk)
            }
            download_chunk_resp::Response::Err(err) => todo!("{err}"),
        };

        debug!(
            "Encrypted chunk {} hash {}",
            ChunkID::from_bytes(chunk_metadata.id.clone().try_into().unwrap()),
            hash_chunk_fn(&chunk_buf).to_string()
        );

        key.decrypt_chunk_in_place(&mut chunk_buf, &chunk_metadata)
            .with_context(|| "Error decrypting chunk")?;

        let chunk_hash = hash_chunk_fn(&chunk_buf);
        let chunk_metadata_hash = ChunkHash::from_bytes(chunk_metadata.hash.try_into().unwrap());

        // Check the hash of the chunk
        if chunk_hash != chunk_metadata_hash {
            todo!("Sent bad chunk")
        }

        // Copy the chunk into the file
        let chunk_byte_index = chunk_metadata.indice * file_header.chunk_size as u64;

        file.seek(std::io::SeekFrom::Start(chunk_byte_index))
            .await
            .with_context(|| {
                anyhow!("Error encounted while seeking to {chunk_byte_index} in file")
            })?;
        file.write_all(&chunk_buf)
            .await
            .with_context(|| "Error while writing chunk to file")?;
    }

    debug!("Finished downloading file {path_to_download_to}");

    Ok(())
}

/// Ensures that the file given and file header as exists in the database are the same
async fn validate_file(file_path: &str, pool: &SqlitePool) -> Result<bool> {
    let file_info =
        sqlx::query("select file_path, hash, chunk_size from files where file_path = ?")
            .bind(file_path)
            .fetch_one(pool)
            .await?;

    let chunk_size: u32 = file_info.get("chunk_size");
    let file_hash: FileHash = file_info.get::<String, _>("hash").try_into()?;

    // TODO: return cstuom error for when file isn't added
    let chunks =
        sqlx::query("select hash, id, chunk_size, indice, nonce from chunks where file_hash = ?")
            .bind(&file_hash)
            .fetch_all(pool)
            .await
            .with_context(|| "Error querying db for chunks: ")?;

    let chunks = chunks.into_iter().map(|chunk| ChunkMetadata {
        id: chunk.get::<String, _>("id").try_into().unwrap(),
        hash: chunk.get::<String, _>("hash").try_into().unwrap(),
        size: chunk.get::<u32, _>("chunk_size"),
        indice: chunk.get::<i64, _>("indice").try_into().unwrap(),
        nonce: chunk.get::<Vec<u8>, _>("nonce").try_into().unwrap(),
    });

    let chunk_indices: HashMap<ChunkID, u64> =
        sqlx::query("select indice, id from chunks where file_hash = ?")
            .bind(&file_hash)
            .fetch_all(pool)
            .await
            .with_context(|| "Error querying db for chunk indicecs: ")?
            .into_iter()
            .map(|chunk_info| {
                let chunk_id: ChunkID = chunk_info.get::<String, _>("id").try_into().unwrap();
                let chunk_indice: u64 = chunk_info.get::<i64, _>("indice").try_into().unwrap();

                (chunk_id, chunk_indice)
            })
            .collect();
    let chunks = chunks
        .into_iter()
        .map(|chunk| {
            (
                ChunkID::from_bytes(chunk.id.clone().try_into().unwrap()),
                chunk,
            )
        })
        .collect();

    let file_header_sql = FileHeader {
        hash: file_hash,
        chunk_size,
        chunks,
        chunk_indices,
    };

    let mut file = File::open(file_path).await?;
    let file_header = FileHeader::from_file(&mut file).await?;

    debug_assert_eq!(file_header_sql.chunk_indices, file_header.chunk_indices);
    debug_assert_eq!(file_header_sql.chunks, file_header.chunks);

    return Ok(file_header_sql == file_header);
}

async fn file_headers_from_path(path: &str, pool: &SqlitePool) -> Result<Vec<FileHeader>> {
    let path = canonicalize(path).await?;
    let path = path.to_string_lossy();

    futures::future::try_join_all(
        sqlx::query("select hash, file_path from files where file_path = ?")
            .bind(path)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|file_info| async move {
                let file_hash: FileHash = file_info.get::<String, _>("hash").try_into()?;
                let file_path = file_info.get::<&str, _>("file_path");

                let chunks = sqlx::query(
                    "select hash, id, chunk_size, indice, nonce from chunks where file_hash = ?",
                )
                .bind(&file_hash)
                .fetch_all(pool)
                .await
                .with_context(|| "Error querying db for chunks: ")?;

                let chunks = chunks.into_iter().map(|chunk| {
                    let chunk_id: ChunkID = chunk.get::<String, _>("id").try_into().unwrap();
                    let chunk_hash: ChunkHash = chunk.get::<String, _>("hash").try_into().unwrap();
                    ChunkMetadata {
                        id: chunk_id.to_bytes().to_vec(),
                        hash: chunk_hash.to_bytes().to_vec(),
                        size: chunk.get::<u32, _>("chunk_size"),
                        indice: chunk.get::<i64, _>("indice").try_into().unwrap(),
                        nonce: chunk.get::<Vec<u8>, _>("nonce").try_into().unwrap(),
                    }
                });

                let chunk_indices: HashMap<ChunkID, u64> =
                    sqlx::query("select indice, id from chunks where file_hash = ?")
                        .bind(&file_hash)
                        .fetch_all(pool)
                        .await
                        .with_context(|| "Error querying db for chunk indicecs: ")?
                        .into_iter()
                        .map(|chunk_info| {
                            let chunk_id: ChunkID =
                                chunk_info.get::<String, _>("id").try_into().unwrap();
                            let chunk_indice: u64 =
                                chunk_info.get::<i64, _>("indice").try_into().unwrap();

                            (chunk_id, chunk_indice)
                        })
                        .collect();

                let chunks = chunks
                    .into_iter()
                    .map(|chunk| {
                        (
                            ChunkID::from_bytes(chunk.id.clone().try_into().unwrap()),
                            chunk,
                        )
                    })
                    .collect();

                let file_info =
                    sqlx::query("select hash, chunk_size from files where file_path = ?")
                        .bind(file_path)
                        .fetch_one(pool)
                        .await?;

                Ok(FileHeader {
                    hash: file_info.get::<String, _>("hash").try_into()?,
                    chunk_size: file_info.get::<u32, _>("chunk_size"),
                    chunks,
                    chunk_indices,
                })
            }),
    )
    .await
}
