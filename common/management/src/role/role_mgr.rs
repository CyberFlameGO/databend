// Copyright 2021 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use common_exception::ErrorCode;
use common_exception::Result;
use common_exception::ToErrorCode;
use common_meta_api::KVApi;
use common_meta_types::GrantObject;
use common_meta_types::IntoSeqV;
use common_meta_types::MatchSeq;
use common_meta_types::MatchSeqExt;
use common_meta_types::OkOrExist;
use common_meta_types::Operation;
use common_meta_types::RoleInfo;
use common_meta_types::SeqV;
use common_meta_types::UpsertKVAction;
use common_meta_types::UserPrivilegeSet;

use crate::role::role_api::RoleMgrApi;

static ROLE_API_KEY_PREFIX: &str = "__fd_roles";

pub struct RoleMgr {
    kv_api: Arc<dyn KVApi>,
    role_prefix: String,
}

impl RoleMgr {
    pub fn new(kv_api: Arc<dyn KVApi>, tenant: &str) -> Self {
        RoleMgr {
            kv_api,
            role_prefix: format!("{}/{}", ROLE_API_KEY_PREFIX, tenant),
        }
    }

    async fn upsert_role_info(
        &self,
        role_info: &RoleInfo,
        seq: Option<u64>,
    ) -> common_exception::Result<u64> {
        let key = format!("{}/{}", self.role_prefix, &role_info.name);
        let value = serde_json::to_vec(&role_info)?;

        let match_seq = match seq {
            None => MatchSeq::GE(1),
            Some(s) => MatchSeq::Exact(s),
        };

        let kv_api = self.kv_api.clone();
        let upsert_kv = async move {
            kv_api
                .upsert_kv(UpsertKVAction::new(
                    &key,
                    match_seq,
                    Operation::Update(value),
                    None,
                ))
                .await
        };
        let res = upsert_kv.await?;
        match res.result {
            Some(SeqV { seq: s, .. }) => Ok(s),
            None => Err(ErrorCode::UnknownRole(format!(
                "unknown role, or seq not match {}",
                role_info.name
            ))),
        }
    }
}

#[async_trait::async_trait]
impl RoleMgrApi for RoleMgr {
    async fn add_role(&self, role_info: &RoleInfo) -> common_exception::Result<u64> {
        let match_seq = MatchSeq::Exact(0);
        let key = format!("{}/{}", self.role_prefix, &role_info.name);
        let value = serde_json::to_vec(&role_info)?;

        let kv_api = self.kv_api.clone();
        let upsert_kv = kv_api.upsert_kv(UpsertKVAction::new(
            &key,
            match_seq,
            Operation::Update(value),
            None,
        ));
        let res = upsert_kv.await?.into_add_result()?;
        match res.res {
            OkOrExist::Ok(v) => Ok(v.seq),
            OkOrExist::Exists(v) => Err(ErrorCode::UserAlreadyExists(format!(
                "Role already exists, seq [{}]",
                v.seq
            ))),
        }
    }

    async fn get_role(&self, role_name: &str, seq: Option<u64>) -> Result<SeqV<RoleInfo>> {
        let key = format!("{}/{}", self.role_prefix, role_name);
        let kv_api = self.kv_api.clone();
        let get_kv = async move { kv_api.get_kv(&key).await };
        let res = get_kv.await?;
        let seq_value =
            res.ok_or_else(|| ErrorCode::UnknownRole(format!("unknown role {}", role_name)))?;

        match MatchSeq::from(seq).match_seq(&seq_value) {
            Ok(_) => Ok(seq_value.into_seqv()?),
            Err(_) => Err(ErrorCode::UnknownRole(format!(
                "unknown role {}",
                role_name
            ))),
        }
    }

    async fn get_roles(&self) -> Result<Vec<SeqV<RoleInfo>>> {
        let role_prefix = self.role_prefix.clone();
        let kv_api = self.kv_api.clone();
        let prefix_list_kv = async move { kv_api.prefix_list_kv(role_prefix.as_str()).await };
        let values = prefix_list_kv.await?;

        let mut r = vec![];
        for (_key, val) in values {
            let u = serde_json::from_slice::<RoleInfo>(&val.data)
                .map_err_to_code(ErrorCode::IllegalUserInfoFormat, || "")?;

            r.push(SeqV::new(val.seq, u));
        }

        Ok(r)
    }

    async fn grant_role_privileges(
        &self,
        role_name: String,
        host_name: String,
        object: GrantObject,
        privileges: UserPrivilegeSet,
        seq: Option<u64>,
    ) -> Result<Option<u64>> {
        let role_val_seq = self.get_role(&role_name, seq);
        let mut role_info = role_val_seq.await?.data;
        role_info
            .grants
            .grant_privileges(&role_name, &host_name, &object, privileges);
        let seq = self.upsert_role_info(&role_info, seq).await?;
        Ok(Some(seq))
    }

    async fn revoke_role_privileges(
        &self,
        role_name: String,
        host_name: String,
        object: GrantObject,
        privileges: UserPrivilegeSet,
        seq: Option<u64>,
    ) -> Result<Option<u64>> {
        let role_val_seq = self.get_role(&role_name, seq);
        let mut role_info = role_val_seq.await?.data;
        role_info
            .grants
            .revoke_privileges(&role_name, &host_name, &object, privileges);
        let seq = self.upsert_role_info(&role_info, seq).await?;
        Ok(Some(seq))
    }

    async fn drop_role(&self, role_name: String, seq: Option<u64>) -> Result<()> {
        let key = format!("{}/{}", self.role_prefix, role_name);
        let kv_api = self.kv_api.clone();
        let upsert_kv = async move {
            kv_api
                .upsert_kv(UpsertKVAction::new(
                    &key,
                    seq.into(),
                    Operation::Delete,
                    None,
                ))
                .await
        };
        let res = upsert_kv.await?;
        if res.prev.is_some() && res.result.is_none() {
            Ok(())
        } else {
            Err(ErrorCode::UnknownRole(format!(
                "unknown role {}",
                role_name
            )))
        }
    }
}
