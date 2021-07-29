// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use sqlx::PgPool;
use warp::{filters::BoxedFilter, Filter, Reply};

use crate::{config::CsrfConfig, templates::Templates};

mod index;
mod login;
mod logout;
mod reauth;

use self::index::filter as index;
use self::login::filter as login;
use self::logout::filter as logout;
use self::reauth::filter as reauth;

pub(super) fn filter(
    pool: PgPool,
    templates: Templates,
    csrf_config: &CsrfConfig,
) -> BoxedFilter<(impl Reply,)> {
    index(pool.clone(), templates.clone(), csrf_config)
        .or(login(pool.clone(), templates.clone(), csrf_config))
        .or(logout(csrf_config))
        .or(reauth(pool, templates, csrf_config))
        .boxed()
}
