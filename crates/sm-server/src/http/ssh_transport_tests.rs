fn plain_ssh_key() -> String {
    use base64::Engine;
    let mut wire = Vec::from(11_u32.to_be_bytes());
    wire.extend(b"ssh-ed25519"); wire.extend(32_u32.to_be_bytes()); wire.extend([9;32]);
    format!("ssh-ed25519 {} test-phone",base64::engine::general_purpose::STANDARD.encode(wire))
}

#[tokio::test]
async fn plain_terminal_ssh_key_import_projection_revocation_and_disabled_routes() {
    let (state,_tmux) = plain_test_state();
    let cfg = &state.config.external_access.studio_ssh;
    let legacy = expand_home(&cfg.legacy_authorized_keys_file);
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy,format!("{}\nssh-rsa invalid\n",plain_ssh_key())).unwrap();
    let (status,preview) = plain_call(&state,Method::GET,"/devices/imported/ssh-keys",json!({})).await;
    assert_eq!(status,StatusCode::OK,"{preview}");
    assert_eq!(preview["import_candidates"].as_array().unwrap().len(),1);
    assert!(!expand_home(&cfg.device_keys_file).exists(),"preview never grants access");
    let (status,added) = plain_call(&state,Method::POST,"/devices/imported/ssh-keys",json!({"import_authorized_keys":true})).await;
    assert_eq!(status,StatusCode::OK,"{added}");
    let fingerprint = added["keys"][0]["fingerprint"].as_str().unwrap().replace('/',"%2F").replace('+',"%2B");
    assert_eq!(fs::read_to_string(expand_home(&cfg.device_keys_file)).unwrap(),format!("restrict,pty {}\n",plain_ssh_key()));
    let (status,body)=plain_call(&state,Method::POST,"/devices/unknown/ssh-keys",json!({"public_key":plain_ssh_key()})).await;
    assert_eq!(status,StatusCode::BAD_REQUEST,"{body}");
    let (status,body)=plain_call(&state,Method::DELETE,&format!("/devices/imported/ssh-keys/{fingerprint}"),json!({})).await;
    assert_eq!(status,StatusCode::OK,"{body}");
    assert!(fs::read_to_string(expand_home(&cfg.device_keys_file)).unwrap().is_empty());
    let id=plain_new(&state).await;
    let (status,body)=plain_call(&state,Method::POST,&format!("/terminals/{id}/route"),json!({"where":"away"})).await;
    assert_eq!(status,StatusCode::CONFLICT,"{body}"); assert_eq!(body["detail"],"no_ssh_key");
    assert!(state.ssh_transport.lock().unwrap().requested_at.is_none());
    let (status,body)=plain_call(&state,Method::POST,&format!("/terminals/{id}/route"),json!({"where":"home"})).await;
    assert_eq!(status,StatusCode::OK,"{body}");
    assert!(body["command"].as_str().unwrap().contains(&format!("tmux -L '{}' attach-session -t {id}",state.runtime().socket_name().unwrap())));
    assert!(state.ssh_transport.lock().unwrap().requested_at.is_none(),"home never requests a tunnel");
    plain_set_limits(&state,json!({"plain_terminal_enabled":false}));
    for (method,path,body) in [
        (Method::GET,"/devices/imported/ssh-keys".into(),json!({})),
        (Method::POST,"/devices/imported/ssh-keys".into(),json!({"public_key":plain_ssh_key()})),
        (Method::DELETE,format!("/devices/imported/ssh-keys/{fingerprint}"),json!({})),
        (Method::POST,format!("/terminals/{id}/route"),json!({"where":"away"})),
    ] {
        let (status,body)=plain_call(&state,method,&path,body).await;
        assert_eq!(status,StatusCode::FORBIDDEN,"{body}"); assert_eq!(body["detail"],"plain_disabled");
    }
}

#[test]
fn plain_terminal_ssh_route_uses_certificate_device_and_rejects_header_spoofing() {
    let signing_key=SigningKey::random(&mut OsRng);
    let mut config=mobile_ticket_config(&signing_key);
    config.cloudflare_access=cloudflare_access_config().cloudflare_access;
    let state=AppState::new(config); seed_cloudflare_access_jwks(&state);
    let mut request=public_request_with_host(Method::POST,"/terminals/term-abcd/route",Body::empty(),"sm-app.example.com");
    request.headers_mut().insert("cf-access-jwt-assertion",test_mobile_access_assertion("test-device").parse().unwrap());
    request.headers_mut().insert("x-sm-device-key-id","someone-else".parse().unwrap());
    assert_eq!(ssh_transport::requesting_device(&state,&request).unwrap().as_deref(),Some("test-device"));
    revoke_mobile_terminal_device_in_state(&state,"local_bypass","test-device").unwrap();
    assert!(ssh_transport::requesting_device(&state,&request).is_err());
}

#[test]
fn plain_terminal_ssh_listener_policy_and_manual_override_are_independent() {
    use crate::studio_ssh::transport::Runtime;
    let mut runtime=Runtime::default();
    assert!(!runtime.tunnel_needed(100,false,true,15));
    runtime.requested_at=Some(100);
    assert!(runtime.tunnel_needed(999,false,true,15));
    assert!(!runtime.tunnel_needed(1000,false,true,15));
    runtime.requested_at=Some(1001);
    assert!(!runtime.tunnel_needed(1002,false,false,15));
    assert!(runtime.tunnel_needed(1002,true,false,15));
}

#[tokio::test]
async fn plain_terminal_ssh_brief_tunnel_login_push_and_saved_log_position() {
    let (mut state,_tmux)=plain_test_state();
    let sender=Arc::new(PlainPushRecorder::default()); state.push_sender=Some(sender.clone());
    follows::push_store(&state).upsert_token(&crate::owner_push::PushTokenRegistration {
        user_id:"rajeshgoli@gmail.com".into(),token:"ssh-token".into(),device_id:Some("phone".into()),device_name:"phone".into(),app_version:"test".into()
    },OffsetDateTime::now_utc()).unwrap();
    let key=ssh_transport::keys(&state).add("imported",&plain_ssh_key()).unwrap();
    let cfg=&state.config.external_access.studio_ssh;
    let assets=expand_home(&cfg.assets_dir); fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("sshd_config"),"ListenAddress 127.0.0.1\n").unwrap();
    // This successful login already exited before the observer's next tick.
    fs::write(assets.join("sshd.err.log"),format!("auth.c:auth_log():306 (bin=sshd-session, pid=2147483000): Accepted publickey for rajesh from 127.0.0.1 port 40000 ssh2: ED25519 {}\n",key.fingerprint)).unwrap();
    state.observe_shell_ssh().unwrap();
    state.observe_shell_ssh().unwrap();
    for _ in 0..40 {
        if !sender.0.lock().unwrap().is_empty() { break; }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let notices=sender.0.lock().unwrap();
    assert_eq!(notices.len(),1); assert_eq!(notices[0]["kind"],"ssh_tunnel_login");
    assert_eq!(notices[0]["body"],"SSH login over tunnel · test-phone");
    let mut restored=crate::studio_ssh::transport::Runtime::load(cfg).unwrap();
    assert!(restored.connections.is_empty());
    assert!(restored.read_log(cfg,OffsetDateTime::now_utc().unix_timestamp()).unwrap().is_empty(),"restart must not resend old logins");
}

#[tokio::test]
async fn plain_terminal_ssh_registration_refuses_revoked_enrollment_after_restart() {
    let signing_key=SigningKey::random(&mut OsRng);
    let mut config=mobile_ticket_config(&signing_key);
    config.rust_core.fixture_writes_enabled=true;
    config.mobile_terminal.allowed_users.get_mut("local_bypass").unwrap().owner=true;
    let state=AppState::new(config);
    let db=mobile_device_db_path(&state.config);
    mobile_devices::create_pairing_registration(&db,"local_bypass",15).unwrap();
    let conn=rusqlite::Connection::open(&db).unwrap();
    conn.execute("INSERT INTO mobile_device_enrollments (user_id,device_id,device_name,public_key_pem,common_name,paired_at,kind) VALUES ('local_bypass','revoked-laptop','Laptop','fixture','revoked-laptop','2026-01-01','computer')",[]).unwrap();
    let endpoint="/devices/revoked-laptop/ssh-keys";
    let (status,body)=plain_call(&state,Method::POST,endpoint,json!({"public_key":plain_ssh_key()})).await;
    assert_eq!(status,StatusCode::OK,"{body}");
    assert!(mobile_devices::revoke_device(&db,"local_bypass","revoked-laptop").unwrap());
    ssh_transport::keys(&state).revoke("revoked-laptop",None).unwrap();
    let restarted=AppState::new(state.config.clone());
    assert!(!mobile_terminal_device_revoked(&restarted,"local_bypass","revoked-laptop").unwrap(),"database tombstone must work without the in-memory configuration-key set");
    let (status,body)=plain_call(&restarted,Method::POST,endpoint,json!({"public_key":plain_ssh_key()})).await;
    assert_eq!(status,StatusCode::BAD_REQUEST,"{body}");
    assert!(ssh_transport::keys(&restarted).list().unwrap().iter().all(|k|k.revoked_at.is_some()));
}
