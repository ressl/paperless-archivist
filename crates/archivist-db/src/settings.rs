//! Runtime settings, prompts and encrypted secrets.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretReferenceView {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub configured: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRecord {
    pub id: Uuid,
    pub stage: Stage,
    pub name: String,
    pub version: i32,
    pub content: String,
    pub output_schema: Option<Value>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptUsageRecord {
    pub prompt_id: Uuid,
    pub run_count: i64,
    pub job_count: i64,
    pub last_used_at: Option<DateTime<Utc>>,
    pub avg_duration_ms: f64,
    pub last_provider: Option<String>,
    pub last_model: Option<String>,
}

/// One row of the prompt A/B experiment evaluation: the review-outcome
/// breakdown for all metadata artifacts stamped with a given
/// `prompt_experiment_group` (see [`get_active_prompt_with_experiment`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptExperimentRecord {
    pub group: String,
    pub total: i64,
    pub approved: i64,
    pub rejected: i64,
    pub edited: i64,
    pub applied: i64,
    pub mean_confidence: Option<f64>,
}

pub async fn get_runtime_settings(pool: &DbPool) -> Result<RuntimeSettings> {
    let row = sqlx::query("select value from settings where key = 'runtime'")
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(RuntimeSettings::default().normalized());
    };
    let value: Value = row.try_get("value")?;
    serde_json::from_value::<RuntimeSettings>(value)
        .map(RuntimeSettings::normalized)
        .context("decode runtime settings")
}

pub async fn update_runtime_settings(
    pool: &DbPool,
    settings: &RuntimeSettings,
    actor_id: Uuid,
) -> Result<()> {
    let after = serde_json::to_value(settings)?;
    let mut tx = pool.begin().await?;
    let before = sqlx::query("select value from settings where key = 'runtime'")
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|row| row.try_get::<Value, _>("value").ok());
    sqlx::query(
        r#"
        insert into settings (key, value, updated_by, updated_at)
        values ('runtime', $1, $2, now())
        on conflict (key)
        do update set value = excluded.value,
                      updated_by = excluded.updated_by,
                      updated_at = now()
        "#,
    )
    .bind(&after)
    .bind(actor_id)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "settings.updated".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before,
            after: Some(after),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_prompts(pool: &DbPool) -> Result<Vec<PromptRecord>> {
    let rows = sqlx::query(
        r#"
        select id, stage, name, version, content, output_schema, active, created_at
          from prompts
         order by stage, name, version desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(prompt_from_row).collect()
}

pub async fn get_active_prompt(pool: &DbPool, stage: Stage) -> Result<Option<PromptRecord>> {
    let row = sqlx::query(
        r#"
        select id, stage, name, version, content, output_schema, active, created_at
          from prompts
         where stage = $1 and active = true and experiment_group is null
         order by created_at desc
         limit 1
        "#,
    )
    .bind(stage.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(prompt_from_row).transpose()
}

/// A/B-experiment-aware variant of [`get_active_prompt`].
///
/// When two active prompts exist for the same (stage, name) — one with
/// `experiment_group='A'`, one with `experiment_group='B'` — picks the
/// variant deterministically from `run_id`. Falls back to the
/// experiment-group-less default (i.e. `get_active_prompt`'s row)
/// when no A/B pair is configured. Returns the variant marker
/// alongside the prompt so the worker can stamp it into
/// `ai_artifacts.normalized.prompt_experiment_group` for downstream
/// accuracy analysis.
pub async fn get_active_prompt_with_experiment(
    pool: &DbPool,
    stage: Stage,
    run_id: Uuid,
) -> Result<Option<(PromptRecord, Option<String>)>> {
    let rows = sqlx::query(
        r#"
        select id, stage, name, version, content, output_schema, active, created_at,
               experiment_group
          from prompts
         where stage = $1 and active = true
         order by case when experiment_group is null then 0 else 1 end,
                  experiment_group nulls first,
                  created_at desc
        "#,
    )
    .bind(stage.to_string())
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }

    let mut default_row: Option<PgRow> = None;
    let mut group_a: Option<PgRow> = None;
    let mut group_b: Option<PgRow> = None;
    for row in rows {
        let group: Option<String> = row.try_get("experiment_group").ok();
        match group.as_deref() {
            None if default_row.is_none() => default_row = Some(row),
            Some("A") => group_a = Some(row),
            Some("B") => group_b = Some(row),
            _ => {}
        }
    }

    let bucket = run_id.as_u128() % 2;
    match (group_a, group_b, default_row) {
        (Some(a), Some(b), _) => {
            // Both groups configured → deterministic 50/50 split on run_id.
            let (row, label) = if bucket == 0 { (a, "A") } else { (b, "B") };
            Ok(Some((prompt_from_row(row)?, Some(label.to_owned()))))
        }
        (_, _, Some(row)) => Ok(Some((prompt_from_row(row)?, None))),
        (Some(row), None, None) => Ok(Some((prompt_from_row(row)?, Some("A".to_owned())))),
        (None, Some(row), None) => Ok(Some((prompt_from_row(row)?, Some("B".to_owned())))),
        (None, None, None) => Ok(None),
    }
}

pub async fn list_prompt_usage(pool: &DbPool) -> Result<Vec<PromptUsageRecord>> {
    let rows = sqlx::query(
        r#"
        select ai.prompt_id,
               count(distinct ai.run_id)::bigint as run_count,
               count(distinct ai.job_id)::bigint as job_count,
               max(ai.created_at) as last_used_at,
               coalesce(avg(ai.duration_ms), 0)::double precision as avg_duration_ms,
               (
                 select latest.provider
                   from ai_artifacts latest
                  where latest.prompt_id = ai.prompt_id
                  order by latest.created_at desc
                  limit 1
               ) as last_provider,
               (
                 select latest.model
                   from ai_artifacts latest
                  where latest.prompt_id = ai.prompt_id
                  order by latest.created_at desc
                  limit 1
               ) as last_model
          from ai_artifacts ai
         where ai.prompt_id is not null
         group by ai.prompt_id
         order by max(ai.created_at) desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(PromptUsageRecord {
                prompt_id: row.try_get("prompt_id")?,
                run_count: row.try_get("run_count")?,
                job_count: row.try_get("job_count")?,
                last_used_at: row.try_get("last_used_at")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
                last_provider: row.try_get("last_provider")?,
                last_model: row.try_get("last_model")?,
            })
        })
        .collect()
}

/// Aggregate prompt A/B-experiment outcomes by `prompt_experiment_group`.
///
/// The group label is stamped into `ai_artifacts.normalized_output ->>
/// 'prompt_experiment_group'` by the metadata worker. We dedupe to the latest
/// artifact per (run, job) — guarding against retries — and join those jobs to
/// their `review_items` to count review statuses per group. `mean_confidence`
/// is the average of the per-artifact mean over the available sub-suggestion
/// confidences (title, document_type, correspondent, document_date, tags,
/// fields). Read-only; no migration.
pub async fn list_prompt_experiments(pool: &DbPool) -> Result<Vec<PromptExperimentRecord>> {
    let rows = sqlx::query(
        r#"
        with art_groups as (
            select distinct on (run_id, job_id)
                   run_id,
                   job_id,
                   normalized_output->>'prompt_experiment_group' as grp,
                   (
                     select avg(c)
                       from (values
                         ((normalized_output#>>'{title,confidence}')::double precision),
                         ((normalized_output#>>'{document_type,confidence}')::double precision),
                         ((normalized_output#>>'{correspondent,confidence}')::double precision),
                         ((normalized_output#>>'{document_date,confidence}')::double precision),
                         ((normalized_output#>>'{tags,confidence}')::double precision),
                         ((normalized_output#>>'{fields,confidence}')::double precision)
                       ) as t(c)
                      where c is not null
                   ) as conf
              from ai_artifacts
             where normalized_output ? 'prompt_experiment_group'
               and normalized_output->>'prompt_experiment_group' is not null
             order by run_id, job_id, created_at desc
        ),
        counts as (
            select ag.grp,
                   count(ri.id) as total,
                   count(ri.id) filter (where ri.status = 'approved') as approved,
                   count(ri.id) filter (where ri.status = 'rejected') as rejected,
                   count(ri.id) filter (where ri.status = 'edited') as edited,
                   count(ri.id) filter (where ri.status = 'applied') as applied
              from art_groups ag
              left join review_items ri
                on ri.run_id = ag.run_id and ri.job_id = ag.job_id
             group by ag.grp
        ),
        conf as (
            select grp, avg(conf)::double precision as mean_confidence
              from art_groups
             group by grp
        )
        select c.grp as grp,
               c.total as total,
               c.approved as approved,
               c.rejected as rejected,
               c.edited as edited,
               c.applied as applied,
               cf.mean_confidence as mean_confidence
          from counts c
          join conf cf using (grp)
         order by c.grp
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(PromptExperimentRecord {
                group: row.try_get("grp")?,
                total: row.try_get("total")?,
                approved: row.try_get("approved")?,
                rejected: row.try_get("rejected")?,
                edited: row.try_get("edited")?,
                applied: row.try_get("applied")?,
                mean_confidence: row.try_get("mean_confidence")?,
            })
        })
        .collect()
}

pub async fn create_prompt(
    pool: &DbPool,
    stage: Stage,
    name: &str,
    content: &str,
    output_schema: Option<Value>,
    actor_id: Uuid,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let version: i32 = sqlx::query(
        "select coalesce(max(version), 0) + 1 as version from prompts where stage = $1 and name = $2",
    )
    .bind(stage.to_string())
    .bind(name)
    .fetch_one(&mut *tx)
    .await?
    .try_get("version")?;
    let id: Uuid = sqlx::query(
        r#"
        insert into prompts (stage, name, version, content, output_schema, active, created_by)
        values ($1, $2, $3, $4, $5, false, $6)
        returning id
        "#,
    )
    .bind(stage.to_string())
    .bind(name)
    .bind(version)
    .bind(content)
    .bind(&output_schema)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "prompt.created".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "prompt_id": id, "stage": stage, "name": name, "version": version }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn activate_prompt(pool: &DbPool, prompt_id: Uuid, actor_id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select stage, name, version from prompts where id = $1")
        .bind(prompt_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(NotFoundError::Prompt)?;
    let stage: String = row.try_get("stage")?;
    let name: String = row.try_get("name")?;
    let version: i32 = row.try_get("version")?;

    sqlx::query("update prompts set active = false where stage = $1 and name = $2")
        .bind(&stage)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update prompts set active = true where id = $1")
        .bind(prompt_id)
        .execute(&mut *tx)
        .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "prompt.activated".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "prompt_id": prompt_id, "stage": stage, "name": name, "version": version }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

fn prompt_from_row(row: PgRow) -> Result<PromptRecord> {
    let stage: String = row.try_get("stage")?;
    Ok(PromptRecord {
        id: row.try_get("id")?,
        stage: stage.parse()?,
        name: row.try_get("name")?,
        version: row.try_get("version")?,
        content: row.try_get("content")?,
        output_schema: row.try_get("output_schema")?,
        active: row.try_get("active")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn list_secret_references(pool: &DbPool) -> Result<Vec<SecretReferenceView>> {
    let rows = sqlx::query(
        r#"
        select id, name, kind, reference, created_at, updated_at
          from secret_references
         order by name
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let reference: Value = row.try_get("reference")?;
            Ok(SecretReferenceView {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                kind: row.try_get("kind")?,
                configured: !reference.is_null(),
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

pub async fn upsert_encrypted_secret(
    pool: &DbPool,
    secret_key: &SecretString,
    name: &str,
    secret: &SecretString,
    actor_id: Uuid,
) -> Result<Uuid> {
    let encrypted = encrypt_secret(secret_key, secret.expose_secret())?;
    let reference = json!({ "ciphertext": encrypted });
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query(
        r#"
        insert into secret_references (name, kind, reference, created_by, updated_by)
        values ($1, 'encrypted_value', $2, $3, $3)
        on conflict (name)
        do update set kind = excluded.kind,
                      reference = excluded.reference,
                      updated_by = excluded.updated_by,
                      updated_at = now()
        returning id
        "#,
    )
    .bind(name)
    .bind(reference)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "secret.changed".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "secret_reference_id": id, "name": name, "kind": "encrypted_value" }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn resolve_secret(
    pool: &DbPool,
    secret_key: &SecretString,
    secret_id: Uuid,
) -> Result<Option<SecretString>> {
    let Some(row) = sqlx::query("select kind, reference from secret_references where id = $1")
        .bind(secret_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let kind: String = row.try_get("kind")?;
    let reference: Value = row.try_get("reference")?;
    let value = match kind.as_str() {
        "encrypted_value" => {
            let ciphertext = reference
                .get("ciphertext")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("encrypted secret reference is missing ciphertext"))?;
            decrypt_secret(secret_key, ciphertext)?
        }
        "env" => {
            let name = reference
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("env secret reference is missing name"))?;
            std::env::var(name).context("read secret from environment")?
        }
        "mounted_file" | "docker_secret" | "kubernetes_secret" => {
            let path = reference
                .get("path")
                .and_then(Value::as_str)
                .or_else(|| reference.get("name").and_then(Value::as_str))
                .ok_or_else(|| anyhow!("file secret reference is missing path/name"))?;
            let resolved = if kind == "docker_secret" && !path.starts_with('/') {
                format!("/run/secrets/{path}")
            } else {
                path.to_owned()
            };
            // File-backed secrets sit on the async hot path; use tokio's non-blocking read so we
            // never stall an executor thread on disk I/O.
            tokio::fs::read_to_string(resolved).await?.trim().to_owned()
        }
        other => return Err(anyhow!("unsupported secret reference kind: {other}")),
    };
    Ok(Some(SecretString::from(value)))
}

pub(crate) fn encrypt_secret(secret_key: &SecretString, plaintext: &str) -> Result<String> {
    let key_bytes = Sha256::digest(secret_key.expose_secret().as_bytes());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_bytes())
        .map_err(|_| anyhow!("encrypt secret"))?;
    let mut packed = nonce_bytes.to_vec();
    packed.extend(ciphertext);
    Ok(BASE64.encode(packed))
}

pub(crate) fn decrypt_secret(secret_key: &SecretString, ciphertext: &str) -> Result<String> {
    let packed = BASE64
        .decode(ciphertext)
        .context("decode encrypted secret")?;
    if packed.len() < 13 {
        return Err(anyhow!("encrypted secret is too short"));
    }
    let (nonce, body) = packed.split_at(12);
    let key_bytes = Sha256::digest(secret_key.expose_secret().as_bytes());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), body)
        .map_err(|_| anyhow!("decrypt secret"))?;
    String::from_utf8(plaintext).context("secret is not utf-8")
}

/// Load one prompt version by id (any activation state). #445
pub async fn get_prompt_by_id(pool: &DbPool, prompt_id: Uuid) -> Result<Option<PromptRecord>> {
    let row = sqlx::query(
        r#"
        select id, stage, name, version, content, output_schema, active, created_at
          from prompts
         where id = $1
        "#,
    )
    .bind(prompt_id)
    .fetch_optional(pool)
    .await?;
    row.map(prompt_from_row).transpose()
}
