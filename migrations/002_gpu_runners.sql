-- M9: GPU-capable runner class support
-- Adds GPU inventory columns to runner_nodes.
-- All columns default to 0/NULL so existing CPU-only runners are unaffected.

ALTER TABLE runner_nodes ADD COLUMN IF NOT EXISTS gpu_model    TEXT;
ALTER TABLE runner_nodes ADD COLUMN IF NOT EXISTS gpu_count    INT NOT NULL DEFAULT 0;
ALTER TABLE runner_nodes ADD COLUMN IF NOT EXISTS gpu_used     INT NOT NULL DEFAULT 0;
ALTER TABLE runner_nodes ADD COLUMN IF NOT EXISTS gpu_vram_mib INT NOT NULL DEFAULT 0;

-- Index for the GPU scheduler query: find healthy runners with free GPU slots
CREATE INDEX IF NOT EXISTS runner_nodes_gpu
    ON runner_nodes (runner_class, healthy, gpu_used, gpu_count)
    WHERE gpu_count > 0;
