// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! A tenant's runner instances: `DescribeInstances` on the
//! `hatchrun:tenant` tag and `TerminateInstances` on several at once,
//! for the reconciler. The tenant role may describe every instance and
//! terminate only tagged ones (deploy/cloudformation/connect.yaml).

use super::ec2::{TAG_JOB_ID, TAG_RUNNER_NAME, TAG_TENANT};
use super::{Error, document, query};
use crate::http::{Request, Response};

/// EC2 API version.
const VERSION: &str = "2016-11-15";
/// Instances in one page, EC2's most.
const PAGE_SIZE: &str = "1000";

/// `tenant`'s instances that are pending or running, a page at a time.
#[must_use]
pub fn describe(endpoint: &str, tenant: &str, next_token: Option<&str>) -> Request {
    let mut params: Vec<(String, String)> = [
        ("Filter.1.Name", format!("tag:{TAG_TENANT}")),
        ("Filter.1.Value.1", tenant.to_owned()),
        ("Filter.2.Name", "instance-state-name".to_owned()),
        ("Filter.2.Value.1", "pending".to_owned()),
        ("Filter.2.Value.2", "running".to_owned()),
        ("MaxResults", PAGE_SIZE.to_owned()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();
    if let Some(token) = next_token {
        params.push(("NextToken".to_owned(), token.to_owned()));
    }
    query(endpoint, "DescribeInstances", VERSION, &params)
}

/// A runner instance as EC2 describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// `i-...`
    pub id: String,
    /// `pending` or `running`.
    pub state: String,
    /// When it started, RFC 3339.
    pub launch_time: String,
    /// Its `hatchrun:job-id` tag.
    pub job_id: Option<String>,
    /// Its `hatchrun:runner-name` tag.
    pub runner_name: Option<String>,
}

/// Reads a page of instances and the next page's token.
///
/// # Errors
///
/// EC2's error, or a malformed answer.
pub fn described(response: &Response) -> Result<(Vec<Described>, Option<String>), Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let instances = doc
        .descendants()
        .filter(|node| node.tag_name().name() == "instancesSet")
        .flat_map(|set| {
            set.children()
                .filter(|node| node.tag_name().name() == "item")
        })
        .map(|item| {
            let tag = |key: &str| {
                child(item, "tagSet")?
                    .children()
                    .filter(|node| node.tag_name().name() == "item")
                    .find(|node| child_text(*node, "key") == Some(key))
                    .and_then(|node| child_text(node, "value"))
                    .map(str::to_owned)
            };
            Ok(Described {
                id: child_text(item, "instanceId")
                    .ok_or_else(|| Error::malformed("instance without an id"))?
                    .to_owned(),
                state: child(item, "instanceState")
                    .and_then(|state| child_text(state, "name"))
                    .unwrap_or_default()
                    .to_owned(),
                launch_time: child_text(item, "launchTime")
                    .unwrap_or_default()
                    .to_owned(),
                job_id: tag(TAG_JOB_ID),
                runner_name: tag(TAG_RUNNER_NAME),
            })
        })
        .collect::<Result<_, Error>>()?;
    let next = child_text(root, "nextToken")
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    Ok((instances, next))
}

/// Terminates several instances in one call.
#[must_use]
pub fn terminate(endpoint: &str, ids: &[&str]) -> Request {
    let params: Vec<(String, String)> = ids
        .iter()
        .enumerate()
        .map(|(at, id)| (format!("InstanceId.{}", at + 1), (*id).to_owned()))
        .collect();
    query(endpoint, "TerminateInstances", VERSION, &params)
}

fn child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    node.children()
        .find(|child| child.tag_name().name() == name)
}

/// The text of a direct child, so an instance's own fields are not
/// confused with those of its network interfaces or volumes.
fn child_text<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(node, name).and_then(|child| child.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_live_tenant_instances() {
        let request = describe("https://ec2.eu-west-1.amazonaws.com/", "acme1234", None);
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            "Action=DescribeInstances&Version=2016-11-15\
             &Filter.1.Name=tag%3Ahatchrun%3Atenant&Filter.1.Value.1=acme1234\
             &Filter.2.Name=instance-state-name&Filter.2.Value.1=pending\
             &Filter.2.Value.2=running&MaxResults=1000"
        );
        let next = describe("https://ec2/", "acme1234", Some("tok/en"));
        assert!(
            String::from_utf8(next.body)
                .unwrap()
                .ends_with("&NextToken=tok%2Fen")
        );
    }

    #[test]
    fn reads_instances_and_their_tags() {
        let answer = Response { headers: Vec::new(),
            status: 200,
            body: br#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>r</requestId>
  <reservationSet>
    <item>
      <reservationId>r-1</reservationId>
      <instancesSet>
        <item>
          <instanceId>i-1</instanceId>
          <instanceState><code>16</code><name>running</name></instanceState>
          <launchTime>2026-10-14T10:00:00.000Z</launchTime>
          <networkInterfaceSet><item><attachment><instanceId>i-other</instanceId></attachment></item></networkInterfaceSet>
          <tagSet>
            <item><key>hatchrun:tenant</key><value>acme1234</value></item>
            <item><key>hatchrun:job-id</key><value>7</value></item>
            <item><key>hatchrun:runner-name</key><value>hatchrun-7-ab</value></item>
          </tagSet>
        </item>
        <item>
          <instanceId>i-2</instanceId>
          <instanceState><code>0</code><name>pending</name></instanceState>
          <launchTime>2026-10-14T10:01:00.000Z</launchTime>
        </item>
      </instancesSet>
    </item>
  </reservationSet>
  <nextToken>page2</nextToken>
</DescribeInstancesResponse>"#
                .to_vec(),
        };
        let (instances, next) = described(&answer).unwrap();
        assert_eq!(next.as_deref(), Some("page2"));
        assert_eq!(
            instances,
            [
                Described {
                    id: "i-1".to_owned(),
                    state: "running".to_owned(),
                    launch_time: "2026-10-14T10:00:00.000Z".to_owned(),
                    job_id: Some("7".to_owned()),
                    runner_name: Some("hatchrun-7-ab".to_owned()),
                },
                Described {
                    id: "i-2".to_owned(),
                    state: "pending".to_owned(),
                    launch_time: "2026-10-14T10:01:00.000Z".to_owned(),
                    job_id: None,
                    runner_name: None,
                },
            ]
        );
    }

    #[test]
    fn an_empty_answer_has_no_instances() {
        let answer = Response { headers: Vec::new(),
            status: 200,
            body: b"<DescribeInstancesResponse><requestId>r</requestId><reservationSet/></DescribeInstancesResponse>"
                .to_vec(),
        };
        assert_eq!(described(&answer).unwrap(), (Vec::new(), None));
    }

    #[test]
    fn terminates_several() {
        let request = terminate("https://ec2/", &["i-1", "i-2"]);
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            "Action=TerminateInstances&Version=2016-11-15&InstanceId.1=i-1&InstanceId.2=i-2"
        );
    }
}
