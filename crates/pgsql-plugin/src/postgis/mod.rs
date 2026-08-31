//! PostGIS plugin — adds GEOMETRY/GEOGRAPHY types and ST_* spatial functions
//! to the schema provider so the checker and linter understand them.

use pgsql_ast::types::DataType;
use pgsql_checker::schema::{FunctionSignature, SchemaProvider, TableSchema};
use std::collections::HashMap;

/// Inferred return type for a PostGIS binary operator.
/// `left` and `right` are the inferred operand types (may be geometry or geography).
fn postgis_operator_return(op: &str, left: &DataType, right: &DataType) -> Option<DataType> {
    let is_spatial = |dt: &DataType| {
        matches!(dt,
            DataType::Custom(s) if s == "geometry" || s == "geography"
        )
    };
    if !is_spatial(left) && !is_spatial(right) {
        return None;
    }
    match op {
        // KNN / distance operators → float8
        "<->" | "<#>" => Some(DataType::DoublePrecision),
        // Bounding box / topological overlap → boolean
        "&&" | "&&&" | "@>" | "<@" | "~" | "@" | "~=" => Some(DataType::Boolean),
        // Directional / geometric predicates → boolean
        "&<" | "&>" | "<<|" | "|>>" | "&<|" | "|&>" | "<^" | ">^" => Some(DataType::Boolean),
        _ => None,
    }
}

/// PostGIS spatial types as custom DataType variants.
pub fn geometry_type() -> DataType {
    DataType::Custom("geometry".into())
}
pub fn geography_type() -> DataType {
    DataType::Custom("geography".into())
}
pub fn box2d_type() -> DataType {
    DataType::Custom("box2d".into())
}
pub fn box3d_type() -> DataType {
    DataType::Custom("box3d".into())
}

/// A SchemaProvider that knows about PostGIS types and ST_* functions.
/// Compose with CompositeSchemaProvider to add PostGIS awareness to any provider.
pub struct PostGisSchemaProvider {
    functions: HashMap<String, FunctionSignature>,
}

impl PostGisSchemaProvider {
    pub fn new() -> Self {
        let mut functions = HashMap::new();

        // Spatial relationship functions → BOOLEAN
        for fname in &[
            "st_equals",
            "st_disjoint",
            "st_intersects",
            "st_touches",
            "st_crosses",
            "st_within",
            "st_contains",
            "st_overlaps",
            "st_covers",
            "st_coveredby",
            "st_dwithin",
        ] {
            functions.insert(
                fname.to_string(),
                FunctionSignature {
                    name: fname.to_string(),
                    params: vec![geometry_type(), geometry_type()],
                    return_type: DataType::Boolean,
                },
            );
        }

        // Measurement functions → DOUBLE PRECISION
        for fname in &[
            "st_distance",
            "st_length",
            "st_perimeter",
            "st_area",
            "st_azimuth",
            "st_angle",
        ] {
            functions.insert(
                fname.to_string(),
                FunctionSignature {
                    name: fname.to_string(),
                    params: vec![geometry_type(), geometry_type()],
                    return_type: DataType::DoublePrecision,
                },
            );
        }

        // Constructor / converter functions → GEOMETRY
        for fname in &[
            "st_geomfromtext",
            "st_geomfromwkb",
            "st_geomfromgeojson",
            "st_point",
            "st_makepoint",
            "st_makeline",
            "st_makepolygon",
            "st_collect",
            "st_union",
            "st_intersection",
            "st_difference",
            "st_buffer",
            "st_simplify",
            "st_transform",
            "st_centroid",
            "st_pointonsurface",
            "st_envelope",
            "st_convexhull",
            "st_boundary",
            "st_exteriorring",
            "st_startpoint",
            "st_endpoint",
            "st_setsrid",
            "st_force2d",
            "st_force3d",
        ] {
            functions.insert(
                fname.to_string(),
                FunctionSignature {
                    name: fname.to_string(),
                    params: vec![geometry_type()],
                    return_type: geometry_type(),
                },
            );
        }

        // Accessor functions → TEXT/INTEGER/BYTEA
        for (fname, ret) in &[
            ("st_astext", DataType::Text),
            ("st_asgeojson", DataType::Text),
            ("st_asbinary", DataType::Bytea),
            ("st_srid", DataType::Integer),
            ("st_ndims", DataType::Integer),
            ("st_npoints", DataType::Integer),
            ("st_geometrytype", DataType::Text),
        ] {
            functions.insert(
                fname.to_string(),
                FunctionSignature {
                    name: fname.to_string(),
                    params: vec![geometry_type()],
                    return_type: ret.clone(),
                },
            );
        }

        // Aggregate functions → GEOMETRY
        for fname in &["st_extent", "st_3dextent", "st_collect", "st_union"] {
            functions.insert(
                fname.to_string(),
                FunctionSignature {
                    name: fname.to_string(),
                    params: vec![geometry_type()],
                    return_type: geometry_type(),
                },
            );
        }

        Self { functions }
    }
}

impl Default for PostGisSchemaProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaProvider for PostGisSchemaProvider {
    fn get_table(&self, _name: &str) -> Option<&TableSchema> {
        None
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        self.functions.get(&name.to_lowercase())
    }

    fn get_operator(&self, op: &str, left: &DataType, right: &DataType) -> Option<DataType> {
        postgis_operator_return(op, left, right)
    }

    fn all_table_names(&self) -> Vec<&str> {
        vec![]
    }
}

/// Plugin implementation for PostGIS.
pub struct PostGisPlugin;

impl crate::plugin::Plugin for PostGisPlugin {
    fn name(&self) -> &'static str {
        "postgis"
    }

    fn schema_provider(&self, _options: &serde_json::Value) -> Option<Box<dyn SchemaProvider>> {
        Some(Box::new(PostGisSchemaProvider::new()))
    }

    fn rules(&self, _options: &serde_json::Value) -> Vec<Box<dyn pgsql_checker::rule::CheckRule>> {
        vec![]
    }
}
