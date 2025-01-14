//  Copyright 2023 Lance Authors
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

#define DUCKDB_BUILD_LOADABLE_EXTENSION
#include "duckdb.h"

extern "C" {

DUCKDB_EXTENSION_API duckdb_logical_type duckdb_create_struct_type(
    idx_t n_pairs, const char** names, const duckdb_logical_type* types);

// https://github.com/duckdb/duckdb/pull/6155/
typedef struct {
	uint64_t offset;
	uint64_t length;
} duckdb_list_entry;

DUCKDB_API void duckdb_list_vector_set_size(duckdb_vector vector, idx_t size);

DUCKDB_API void duckdb_list_vector_reserve(duckdb_vector vector, idx_t required_capacity);

};
