# Block Feature Implementation Guide

## Status: PARTIAL IMPLEMENTATION

The foundation has been laid for the block feature. Below is the status and remaining work.

## ✅ Completed

### Phase 1: Foundation
- ✅ `types/src/block.rs` - All block-related types
- ✅ `types/src/flow.rs` - Updated Flow to include blocks
- ✅ `types/src/lib.rs` - Exported block types
- ✅ `backend/src/blocks/mod.rs` - Module structure
- ✅ `backend/src/blocks/builtin.rs` - AES67 Input/Output blocks
- ✅ `backend/src/blocks/storage.rs` - JSON persistence
- ✅ `backend/src/blocks/registry.rs` - BlockRegistry with tests

### Phase 2: API
- ✅ `backend/src/api/blocks.rs` - Full CRUD handlers
- ✅ `backend/src/api/mod.rs` - Added blocks module
- ✅ `backend/src/lib.rs` - Added block routes

## 🔄 In Progress / Remaining

### Phase 2: Complete Backend Integration

#### 1. Update `backend/src/state.rs`

Add BlockRegistry to AppState:

```rust
use crate::blocks::BlockRegistry;

struct AppStateInner {
    flows: RwLock<HashMap<FlowId, Flow>>,
    storage: Arc<dyn Storage>,
    element_discovery: RwLock<ElementDiscovery>,
    pipelines: RwLock<HashMap<FlowId, PipelineManager>>,
    events: EventBroadcaster,
    block_registry: BlockRegistry,  // ADD THIS
}

impl AppState {
    pub fn new(storage: impl Storage + 'static, blocks_path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                flows: RwLock::new(HashMap::new()),
                storage: Arc::new(storage),
                element_discovery: RwLock::new(ElementDiscovery::new()),
                pipelines: RwLock::new(HashMap::new()),
                events: EventBroadcaster::default(),
                block_registry: BlockRegistry::new(blocks_path),  // ADD THIS
            }),
        }
    }

    pub fn blocks(&self) -> &BlockRegistry {
        &self.inner.block_registry
    }

    pub async fn load_from_storage(&self) -> anyhow::Result<()> {
        // ... existing flow loading ...

        // ADD THIS: Load blocks
        if let Err(e) = self.inner.block_registry.load_user_blocks().await {
            error!("Failed to load user blocks: {}", e);
        }

        Ok(())
    }
}
```

#### 2. Update `backend/src/openapi.rs`

Add block-related schemas:

```rust
use crate::api::blocks;

#[derive(OpenApi)]
#[openapi(
    paths(
        // ... existing paths ...
        blocks::list_blocks,
        blocks::get_block,
        blocks::create_block,
        blocks::update_block,
        blocks::delete_block,
        blocks::get_categories,
    ),
    components(
        schemas(
            // ... existing schemas ...
            strom_types::BlockDefinition,
            strom_types::BlockInstance,
            strom_types::ExposedProperty,
            strom_types::ExternalPad,
            strom_types::ExternalPads,
            strom_types::PropertyMapping,
            strom_types::PropertyType,
            strom_types::BlockResponse,
            strom_types::BlockListResponse,
            strom_types::CreateBlockRequest,
            strom_types::BlockCategoriesResponse,
        )
    ),
    tags(
        (name = "blocks", description = "Block management endpoints")
    )
)]
pub struct ApiDoc;
```

#### 3. Update `backend/src/main.rs`

Initialize block registry:

```rust
let state = AppState::new(
    JsonFileStorage::new(&config.storage.path),
    "blocks.json"  // ADD THIS
);

// Load flows and blocks
state.load_from_storage().await?;
```

### Phase 3: Pipeline Block Expansion

#### Update `backend/src/gst/pipeline.rs`

Add block expansion logic to PipelineManager::new():

```rust
impl PipelineManager {
    pub fn new(
        flow: &Flow,
        events: EventBroadcaster,
        block_registry: &BlockRegistry  // ADD THIS PARAMETER
    ) -> Result<Self, PipelineError> {
        info!("Creating pipeline for flow: {} ({})", flow.name, flow.id);

        let pipeline = gst::Pipeline::builder()
            .name(format!("flow-{}", flow.id))
            .build();

        let mut manager = Self {
            flow_id: flow.id,
            flow_name: flow.name.clone(),
            pipeline,
            elements: HashMap::new(),
            bus_watch: None,
            events,
            pending_links: Vec::new(),
        };

        // EXPAND BLOCKS INTO ELEMENTS
        let (expanded_elements, expanded_links) =
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    Self::expand_blocks(
                        &flow.blocks,
                        &flow.elements,
                        &flow.links,
                        block_registry
                    ).await
                })
            })?;

        // Create all elements (original + from blocks)
        for element in &expanded_elements {
            manager.add_element(element)?;
        }

        // Rest of implementation...

        Ok(manager)
    }

    async fn expand_blocks(
        blocks: &[BlockInstance],
        elements: &[Element],
        links: &[Link],
        registry: &BlockRegistry,
    ) -> Result<(Vec<Element>, Vec<Link>), PipelineError> {
        let mut expanded_elements = elements.to_vec();
        let mut expanded_links = Vec::new();

        for block_instance in blocks {
            // Get block definition
            let definition = registry
                .get_by_id(&block_instance.block_definition_id)
                .await
                .ok_or_else(|| {
                    PipelineError::InvalidFlow(format!(
                        "Block definition not found: {}",
                        block_instance.block_definition_id
                    ))
                })?;

            // Namespace internal elements with block instance ID
            for internal_elem in &definition.elements {
                let mut namespaced_elem = internal_elem.clone();
                namespaced_elem.id = format!("{}:{}", block_instance.id, internal_elem.id);
                namespaced_elem.position = None;

                // Apply exposed property mappings
                for (prop_name, prop_value) in &block_instance.properties {
                    if let Some(exposed_prop) = definition
                        .exposed_properties
                        .iter()
                        .find(|p| p.name == *prop_name)
                    {
                        if exposed_prop.mapping.element_id == internal_elem.id {
                            namespaced_elem.properties.insert(
                                exposed_prop.mapping.property_name.clone(),
                                prop_value.clone(),
                            );
                        }
                    }
                }

                expanded_elements.push(namespaced_elem);
            }

            // Namespace internal links
            for internal_link in &definition.internal_links {
                expanded_links.push(Link {
                    from: Self::namespace_pad(&block_instance.id, &internal_link.from),
                    to: Self::namespace_pad(&block_instance.id, &internal_link.to),
                });
            }
        }

        // Resolve external links (block pads to elements or other blocks)
        for link in links {
            let from = Self::resolve_pad(link.from.as_str(), blocks, registry).await?;
            let to = Self::resolve_pad(link.to.as_str(), blocks, registry).await?;
            expanded_links.push(Link { from, to });
        }

        Ok((expanded_elements, expanded_links))
    }

    fn namespace_pad(block_id: &str, pad_ref: &str) -> String {
        format!("{}:{}", block_id, pad_ref)
    }

    async fn resolve_pad(
        pad_ref: &str,
        blocks: &[BlockInstance],
        registry: &BlockRegistry,
    ) -> Result<String, PipelineError> {
        // Check if this references a block's external pad
        for block_instance in blocks {
            if pad_ref.starts_with(&format!("{}:", block_instance.id)) {
                let parts: Vec<&str> = pad_ref.splitn(2, ':').collect();
                if parts.len() == 2 {
                    let external_pad_name = parts[1];

                    // Get block definition
                    let definition = registry
                        .get_by_id(&block_instance.block_definition_id)
                        .await
                        .ok_or_else(|| {
                            PipelineError::InvalidFlow(format!(
                                "Block definition not found: {}",
                                block_instance.block_definition_id
                            ))
                        })?;

                    // Find matching external pad
                    let all_pads: Vec<&ExternalPad> = definition
                        .external_pads
                        .inputs
                        .iter()
                        .chain(definition.external_pads.outputs.iter())
                        .collect();

                    if let Some(external_pad) = all_pads.iter().find(|p| p.name == external_pad_name) {
                        // Return namespaced internal pad
                        return Ok(format!(
                            "{}:{}:{}",
                            block_instance.id,
                            external_pad.internal_element_id,
                            external_pad.internal_pad_name
                        ));
                    }
                }
            }
        }

        // Not a block reference, return as-is
        Ok(pad_ref.to_string())
    }
}
```

Update `backend/src/state.rs` to pass block_registry to PipelineManager:

```rust
pub async fn start_flow(&self, id: &FlowId) -> Result<PipelineState, PipelineError> {
    // ... existing code ...

    // Create pipeline with event broadcaster AND block registry
    let mut manager = PipelineManager::new(
        &flow,
        self.inner.events.clone(),
        &self.inner.block_registry  // ADD THIS
    )?;

    // ... rest of implementation ...
}
```

### Phase 4-6: Frontend (Simplified Guidance)

Due to implementation complexity, here's a high-level guide:

#### 1. Frontend API Client (`frontend/src/blocks.rs`)

```rust
// Create API client methods for blocks
pub async fn fetch_blocks() -> Result<Vec<BlockDefinition>, String> {
    let url = format!("{}/api/blocks", API_BASE);
    // ... fetch implementation ...
}
```

#### 2. Update Palette (`frontend/src/palette.rs`)

- Add "Blocks" section
- Load blocks from API
- Display with icons
- Allow drag-and-drop

#### 3. Graph Rendering (`frontend/src/graph.rs`)

- Render BlockInstance differently from Element
- Show external pads only
- Use block UI metadata (color, icon)

#### 4. Property Inspector

- Show exposed properties for selected block
- Map to block instance properties

### Phase 7: Additional Built-in Blocks

Add to `backend/src/blocks/builtin.rs`:

```rust
fn rtmp_output_block() -> BlockDefinition { /* ... */ }
fn hls_output_block() -> BlockDefinition { /* ... */ }
fn ndi_input_block() -> BlockDefinition { /* ... */ }
// etc.
```

### Phase 8: Documentation

Update:
- README.md - Add block feature section
- CHANGELOG.md - Document new feature
- Create example blocks.json

## Testing

```bash
# Run backend tests
cargo test --package strom-backend

# Test block registry
cargo test --package strom-backend --lib blocks::registry

# Test API
cargo run -p strom-backend
# Visit http://localhost:8080/swagger-ui
```

## Example blocks.json

```json
{
  "blocks": [
    {
      "id": "user.custom_block_123",
      "name": "My Custom Block",
      "description": "Custom user block",
      "category": "Custom",
      "elements": [],
      "internal_links": [],
      "exposed_properties": [],
      "external_pads": {
        "inputs": [],
        "outputs": []
      },
      "built_in": false,
      "ui_metadata": null
    }
  ]
}
```

## Next Steps

1. Complete state.rs updates
2. Update openapi.rs
3. Update main.rs
4. Implement block expansion in pipeline.rs
5. Basic frontend support
6. Test with real GStreamer pipelines
7. Add more built-in blocks
8. Document feature

## Notes

- Block expansion happens at pipeline creation time
- Blocks are purely a configuration abstraction
- At runtime, everything becomes native GStreamer elements
- Block IDs with "user." prefix are user-defined
- Block IDs with "builtin." prefix are read-only
