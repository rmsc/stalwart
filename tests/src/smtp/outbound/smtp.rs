/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::time::{Duration, Instant};

use common::{config::server::ServerProtocol, ipc::QueueEvent};
use mail_auth::MX;
use store::write::now;

use crate::smtp::{
    DnsCache, TestSMTP,
    inbound::{TestMessage, TestQueueEvent},
    session::{TestSession, VerifyResponse},
};
use smtp::queue::spool::SmtpSpool;

const LOCAL: &str = r#"
[session.rcpt]
relay = true
max-recipients = 100

[session.extensions]
dsn = true

[queue.schedule]
retry = "1s"
notify = [{if = "rcpt_domain = 'foobar.org'", then = "[1s, 2s]"},
          {if = "rcpt_domain = 'foobar.com'", then = "[5s, 6s]"},
          {else = [1s]}]
expire = [{if = "rcpt_domain = 'foobar.org'", then = "6s"},
          {else = "7s"}]
"#;

const REMOTE: &str = r#"
[session.ehlo]
reject-non-fqdn = false

[session.rcpt]
relay = true

[session.extensions]
dsn = true
chunking = false
"#;

const SMUGGLER: &str = r#"From: Joe SixPack <john@foobar.net>
To: Suzie Q <suzie@foobar.org>
Subject: Is dinner ready?

Hi.

We lost the game. Are you hungry yet?
.hey
Joe.

<SEP>.
MAIL FROM:<admin@foobar.net>
RCPT TO:<ok@foobar.org>
DATA
From: Joe SixPack <admin@foobar.net>
To: Suzie Q <suzie@foobar.org>
Subject: smuggled message

This is a smuggled message
"#;

#[tokio::test]
#[serial_test::serial]
async fn smtp_delivery() {
    // Enable logging
    crate::enable_logging();

    // Start test server
    let mut remote = TestSMTP::new("smtp_delivery_remote", REMOTE).await;
    let _rx = remote.start(&[ServerProtocol::Smtp]).await;
    let remote_core = remote.build_smtp();

    // Multiple delivery attempts
    let mut local = TestSMTP::new("smtp_delivery_local", LOCAL).await;

    // Add mock DNS entries
    let core = local.build_smtp();
    for domain in ["foobar.org", "foobar.net", "foobar.com"] {
        core.mx_add(
            domain,
            vec![MX {
                exchanges: vec![format!("mx1.{domain}"), format!("mx2.{domain}")],
                preference: 10,
            }],
            Instant::now() + Duration::from_secs(10),
        );
        core.ipv4_add(
            format!("mx1.{domain}"),
            vec!["127.0.0.1".parse().unwrap()],
            Instant::now() + Duration::from_secs(30),
        );
        core.ipv4_add(
            format!("mx2.{domain}"),
            vec!["127.0.0.1".parse().unwrap()],
            Instant::now() + Duration::from_secs(30),
        );
    }

    let mut session = local.new_session();
    session.data.remote_ip_str = "10.0.0.1".into();
    session.eval_session_params().await;
    session.ehlo("mx.test.org").await;
    session
        .send_message(
            "john@test.org",
            &[
                "<ok@foobar.org> NOTIFY=SUCCESS,DELAY,FAILURE",
                "<delay@foobar.org> NOTIFY=SUCCESS,DELAY,FAILURE",
                "<fail@foobar.org> NOTIFY=SUCCESS,DELAY,FAILURE",
                "<ok@foobar.net> NOTIFY=SUCCESS,DELAY,FAILURE",
                "<delay@foobar.net> NOTIFY=SUCCESS,DELAY,FAILURE",
                "<fail@foobar.net> NOTIFY=SUCCESS,DELAY,FAILURE",
                "<invalid@domain.org> NOTIFY=SUCCESS,DELAY,FAILURE",
            ],
            "test:no_dkim",
            "250",
        )
        .await;
    let message = local.queue_receiver.expect_message().await;
    let num_domains = message.domains.len();
    assert_eq!(num_domains, 3);
    local
        .queue_receiver
        .delivery_attempt(message.queue_id)
        .await
        .try_deliver(core.clone());
    let mut dsn = Vec::new();
    let mut domain_retries = vec![0; num_domains];
    loop {
        match local.queue_receiver.try_read_event().await {
            Some(QueueEvent::Refresh | QueueEvent::WorkerDone { .. }) => {}
            Some(QueueEvent::Paused(_)) => unreachable!(),
            None | Some(QueueEvent::Stop) => break,
        }

        let events = core.next_event().await;
        if events.is_empty() {
            break;
        }
        let now = now();
        for event in events {
            if event.due > now {
                tokio::time::sleep(Duration::from_secs(event.due - now)).await;
            }

            let message = core.read_message(event.queue_id).await.unwrap();
            if message.return_path.is_empty() {
                message.clone().remove(&core, event.due).await;
                dsn.push(message);
            } else {
                for (idx, domain) in message.domains.iter().enumerate() {
                    domain_retries[idx] = domain.retry.inner;
                }
                event.try_deliver(core.clone());
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    assert_eq!(domain_retries[0], 0, "retries {domain_retries:?}");
    assert!(domain_retries[1] >= 5, "retries {domain_retries:?}");
    assert!(domain_retries[2] >= 5, "retries {domain_retries:?}");
    assert!(
        domain_retries[1] >= domain_retries[2],
        "retries {domain_retries:?}"
    );

    local.queue_receiver.assert_queue_is_empty().await;
    assert_eq!(dsn.len(), 5);

    let mut dsn = dsn.into_iter();

    dsn.next()
        .unwrap()
        .read_lines(&local.queue_receiver)
        .await
        .assert_contains("<ok@foobar.net> (delivered to")
        .assert_contains("<ok@foobar.org> (delivered to")
        .assert_contains("<invalid@domain.org> (failed to lookup")
        .assert_contains("<fail@foobar.net> (host ")
        .assert_contains("<fail@foobar.org> (host ");

    dsn.next()
        .unwrap()
        .read_lines(&local.queue_receiver)
        .await
        .assert_contains("<delay@foobar.net> (host ")
        .assert_contains("<delay@foobar.org> (host ")
        .assert_contains("Action: delayed");

    dsn.next()
        .unwrap()
        .read_lines(&local.queue_receiver)
        .await
        .assert_contains("<delay@foobar.org> (host ")
        .assert_contains("Action: delayed");

    dsn.next()
        .unwrap()
        .read_lines(&local.queue_receiver)
        .await
        .assert_contains("<delay@foobar.org> (host ");

    dsn.next()
        .unwrap()
        .read_lines(&local.queue_receiver)
        .await
        .assert_contains("<delay@foobar.net> (host ")
        .assert_contains("Action: failed");

    assert_eq!(
        remote
            .queue_receiver
            .consume_message(&remote_core)
            .await
            .recipients
            .into_iter()
            .map(|r| r.address)
            .collect::<Vec<_>>(),
        vec!["ok@foobar.org".to_string()]
    );
    assert_eq!(
        remote
            .queue_receiver
            .consume_message(&remote_core)
            .await
            .recipients
            .into_iter()
            .map(|r| r.address)
            .collect::<Vec<_>>(),
        vec!["ok@foobar.net".to_string()]
    );

    remote.queue_receiver.assert_no_events();

    // SMTP smuggling
    for separator in ["\n", "\r"].iter() {
        session.data.remote_ip_str = "10.0.0.2".into();
        session.eval_session_params().await;
        session.ehlo("mx.test.org").await;

        let message = SMUGGLER
            .replace('\r', "")
            .replace('\n', "\r\n")
            .replace("<SEP>", separator);

        session
            .send_message("john@doe.org", &["bill@foobar.com"], &message, "250")
            .await;
        local
            .queue_receiver
            .expect_message_then_deliver()
            .await
            .try_deliver(core.clone());
        local
            .queue_receiver
            .read_event()
            .await
            .assert_refresh_or_done();

        let message = remote
            .queue_receiver
            .consume_message(&remote_core)
            .await
            .read_message(&remote.queue_receiver)
            .await;

        assert!(
            message.contains("This is a smuggled message"),
            "message: {:?}",
            message
        );
        assert!(
            message.contains("We lost the game."),
            "message: {:?}",
            message
        );
        assert!(
            message.contains(&format!("{separator}..\r\nMAIL FROM:<",)),
            "message: {:?}",
            message
        );
    }
}
