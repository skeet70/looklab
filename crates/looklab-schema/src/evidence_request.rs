// SPDX-License-Identifier: AGPL-3.0-or-later
//! Public evidence-request wire records shared by the server API and the web client.

use serde::{Deserialize, Serialize};

use crate::ids::{BodyId, SimKey};
use crate::model_subject::{ControlEpochId, ModelSubject};
use crate::qualification::{
    EvidenceRequestKind, ModelReportId, QualificationCapability, SubjectKey,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum EvidenceRequestOrigin {
    Report { report: ModelReportId },
    PreCandidate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRequestWire {
    #[serde(flatten)]
    pub origin: EvidenceRequestOrigin,
    pub subject: ModelSubject,
    pub capability: QualificationCapability,
    pub kind: EvidenceRequestKind,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_groups: Option<u32>,
}

impl EvidenceRequestWire {
    pub fn subject_key(&self) -> SubjectKey {
        self.subject.key()
    }

    pub fn report(&self) -> Option<&ModelReportId> {
        match &self.origin {
            EvidenceRequestOrigin::Report { report } => Some(report),
            EvidenceRequestOrigin::PreCandidate => None,
        }
    }

    pub fn body(&self) -> Option<&BodyId> {
        match &self.subject {
            ModelSubject::BodyRealization { realization } => Some(&realization.body),
            ModelSubject::SharedFamily { .. } => None,
        }
    }

    pub fn epoch(&self) -> Option<ControlEpochId> {
        match &self.subject {
            ModelSubject::BodyRealization { realization } => Some(realization.epoch),
            ModelSubject::SharedFamily { .. } => None,
        }
    }

    pub fn native_sim(&self) -> Option<&SimKey> {
        match &self.subject {
            ModelSubject::BodyRealization { realization } => Some(&realization.native_sim),
            ModelSubject::SharedFamily { .. } => None,
        }
    }
}
