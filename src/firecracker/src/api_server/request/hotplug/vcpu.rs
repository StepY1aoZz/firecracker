// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use micro_http::Body;
use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::vcpu_hotplug::HotplugVcpuConfig;

use super::super::super::parsed_request::{ParsedRequest, RequestError};

pub(crate) fn parse_patch_vcpu_hotplug(body: &Body) -> Result<ParsedRequest, RequestError> {
    let config = serde_json::from_slice::<HotplugVcpuConfig>(body.raw())?;
    Ok(ParsedRequest::new_sync(VmmAction::HotplugVcpus(config)))
}

#[cfg(test)]
mod tests {
    use vmm::VcpuAffinity;

    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_patch_vcpu_hotplug() {
        let body = Body::new(
            r#"{
                "add": 2,
                "affinity": [{"vcpu_id": 2, "host_cpus": [4, 5]}]
            }"#,
        );
        let expected = HotplugVcpuConfig {
            add: 2,
            affinity: vec![VcpuAffinity {
                vcpu_id: 2,
                host_cpus: vec![4, 5],
            }],
        };
        assert_eq!(
            vmm_action_from_request(parse_patch_vcpu_hotplug(&body).unwrap()),
            VmmAction::HotplugVcpus(expected)
        );

        parse_patch_vcpu_hotplug(&Body::new("invalid_payload")).unwrap_err();
        parse_patch_vcpu_hotplug(&Body::new(r#"{"add": 1, "unknown": 0}"#)).unwrap_err();
    }
}
