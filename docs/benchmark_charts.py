#!/usr/bin/env python3
"""
CubeSandbox Benchmark Comparison Charts
Baseline (standard snapshot) vs Layered Snapshot
"""

import matplotlib.pyplot as plt
import numpy as np

# ============================================================
# Data
# ============================================================

# §3.2 Startup Latency - Average (ms)
concurrency = [1, 10, 20, 50]
baseline_avg = [285.9, 881.5, 1269.1, 2840.1]
layered_avg = [306.8, 823.6, 1349.4, 2775.2]

# §3.2 Startup Latency - P95 (ms)
baseline_p95 = [313.7, 1272.6, 1370.8, 3246.0]
layered_p95 = [438.2, 1389.8, 1455.6, 3113.4]

# §3.3 Memory Density (MB per sandbox)
density_batch = [10, 20, 50, 100]
baseline_mem = [18.5, 24.2, 24.8, 24.9]
layered_mem = [21.4, 20.7, 24.9, 26.5]

# ============================================================
# Style
# ============================================================
plt.rcParams.update({
    'font.family': 'DejaVu Sans',
    'font.size': 11,
    'axes.titlesize': 13,
    'axes.labelsize': 11,
    'legend.fontsize': 10,
})

COLOR_BASELINE = '#4C72B0'
COLOR_LAYERED = '#DD8452'

# ============================================================
# Figure 1: §3.2 Startup Latency
# ============================================================
fig1, axes1 = plt.subplots(1, 2, figsize=(14, 5.5))
fig1.suptitle('§3.2  Sandbox Startup Latency — Baseline vs Layered Snapshot',
              fontsize=14, fontweight='bold', y=1.02)

x = np.arange(len(concurrency))
width = 0.32

# -- Average --
ax = axes1[0]
bars1 = ax.bar(x - width/2, baseline_avg, width, label='Baseline', color=COLOR_BASELINE, edgecolor='white')
bars2 = ax.bar(x + width/2, layered_avg, width, label='Layered Snapshot', color=COLOR_LAYERED, edgecolor='white')
ax.set_xlabel('Concurrency')
ax.set_ylabel('Average Latency (ms)')
ax.set_title('Average Latency')
ax.set_xticks(x)
ax.set_xticklabels(concurrency)
ax.legend()
ax.grid(axis='y', alpha=0.3)

for bar in bars1:
    ax.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 30,
            f'{bar.get_height():.0f}', ha='center', va='bottom', fontsize=9, color=COLOR_BASELINE)
for bar in bars2:
    ax.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 30,
            f'{bar.get_height():.0f}', ha='center', va='bottom', fontsize=9, color=COLOR_LAYERED)

# -- P95 --
ax = axes1[1]
bars1 = ax.bar(x - width/2, baseline_p95, width, label='Baseline', color=COLOR_BASELINE, edgecolor='white')
bars2 = ax.bar(x + width/2, layered_p95, width, label='Layered Snapshot', color=COLOR_LAYERED, edgecolor='white')
ax.set_xlabel('Concurrency')
ax.set_ylabel('P95 Latency (ms)')
ax.set_title('P95 Latency')
ax.set_xticks(x)
ax.set_xticklabels(concurrency)
ax.legend()
ax.grid(axis='y', alpha=0.3)

for bar in bars1:
    ax.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 30,
            f'{bar.get_height():.0f}', ha='center', va='bottom', fontsize=9, color=COLOR_BASELINE)
for bar in bars2:
    ax.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 30,
            f'{bar.get_height():.0f}', ha='center', va='bottom', fontsize=9, color=COLOR_LAYERED)

fig1.tight_layout()
fig1.savefig('/users/liufy/Experiment/CubeSandbox/docs/fig3_2_startup_latency.png',
             dpi=150, bbox_inches='tight', facecolor='white')
print("Saved fig3_2_startup_latency.png")

# ============================================================
# Figure 2: §3.3 Memory Density
# ============================================================
fig2, ax2 = plt.subplots(figsize=(8, 5.5))
fig2.suptitle('§3.3  Per-Sandbox Memory Overhead — Baseline vs Layered Snapshot',
              fontsize=14, fontweight='bold', y=1.02)

x2 = np.arange(len(density_batch))
bars1 = ax2.bar(x2 - width/2, baseline_mem, width, label='Baseline', color=COLOR_BASELINE, edgecolor='white')
bars2 = ax2.bar(x2 + width/2, layered_mem, width, label='Layered Snapshot', color=COLOR_LAYERED, edgecolor='white')

ax2.set_xlabel('Batch Size (sandboxes)')
ax2.set_ylabel('Memory per Sandbox (MB)')
ax2.set_title('Memory Density')
ax2.set_xticks(x2)
ax2.set_xticklabels(density_batch)
ax2.legend()
ax2.grid(axis='y', alpha=0.3)
ax2.set_ylim(0, 32)

for bar in bars1:
    ax2.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 0.3,
             f'{bar.get_height():.1f}', ha='center', va='bottom', fontsize=9, color=COLOR_BASELINE)
for bar in bars2:
    ax2.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 0.3,
             f'{bar.get_height():.1f}', ha='center', va='bottom', fontsize=9, color=COLOR_LAYERED)

fig2.tight_layout()
fig2.savefig('/users/liufy/Experiment/CubeSandbox/docs/fig3_3_memory_density.png',
             dpi=150, bbox_inches='tight', facecolor='white')
print("Saved fig3_3_memory_density.png")

# ============================================================
# Figure 3: Improvement Percentage (combined)
# ============================================================
fig3, axes3 = plt.subplots(1, 2, figsize=(14, 5.5))
fig3.suptitle('Layered Snapshot Improvement over Baseline (%)',
              fontsize=14, fontweight='bold', y=1.02)

# -- Latency improvement --
latency_improvement = [(b - l) / b * 100 for b, l in zip(baseline_avg, layered_avg)]
p95_improvement = [(b - l) / b * 100 for b, l in zip(baseline_p95, layered_p95)]

ax = axes3[0]
colors_lat = [COLOR_LAYERED if v >= 0 else '#c44e52' for v in latency_improvement]
colors_p95 = [COLOR_LAYERED if v >= 0 else '#c44e52' for v in p95_improvement]

x = np.arange(len(concurrency))
bars1 = ax.bar(x - width/2, latency_improvement, width, label='Avg Latency', color=colors_lat, edgecolor='white', alpha=0.85)
bars2 = ax.bar(x + width/2, p95_improvement, width, label='P95 Latency', color=colors_p95, edgecolor='white', alpha=0.85)

ax.axhline(y=0, color='black', linewidth=0.8)
ax.set_xlabel('Concurrency')
ax.set_ylabel('Improvement (%)')
ax.set_title('Startup Latency Improvement\n(positive = layered is faster)')
ax.set_xticks(x)
ax.set_xticklabels(concurrency)
ax.legend()
ax.grid(axis='y', alpha=0.3)

for bar in bars1:
    y = bar.get_height()
    offset = 0.5 if y >= 0 else -1.5
    ax.text(bar.get_x() + bar.get_width()/2, y + offset,
            f'{y:+.1f}%', ha='center', va='bottom' if y >= 0 else 'top', fontsize=9)
for bar in bars2:
    y = bar.get_height()
    offset = 0.5 if y >= 0 else -1.5
    ax.text(bar.get_x() + bar.get_width()/2, y + offset,
            f'{y:+.1f}%', ha='center', va='bottom' if y >= 0 else 'top', fontsize=9)

# -- Memory improvement --
mem_improvement = [(b - l) / b * 100 for b, l in zip(baseline_mem, layered_mem)]

ax = axes3[1]
colors_mem = [COLOR_LAYERED if v >= 0 else '#c44e52' for v in mem_improvement]

x2 = np.arange(len(density_batch))
bars = ax.bar(x2, mem_improvement, width=0.5, color=colors_mem, edgecolor='white', alpha=0.85)

ax.axhline(y=0, color='black', linewidth=0.8)
ax.set_xlabel('Batch Size')
ax.set_ylabel('Improvement (%)')
ax.set_title('Memory Overhead Improvement\n(positive = layered uses less memory)')
ax.set_xticks(x2)
ax.set_xticklabels(density_batch)
ax.grid(axis='y', alpha=0.3)

for bar in bars:
    y = bar.get_height()
    offset = 0.5 if y >= 0 else -1.5
    ax.text(bar.get_x() + bar.get_width()/2, y + offset,
            f'{y:+.1f}%', ha='center', va='bottom' if y >= 0 else 'top', fontsize=9)

fig3.tight_layout()
fig3.savefig('/users/liufy/Experiment/CubeSandbox/docs/fig_improvement.png',
             dpi=150, bbox_inches='tight', facecolor='white')
print("Saved fig_improvement.png")

print("\nAll charts generated successfully!")
