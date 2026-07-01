---
sidebar_position: 12
title: Replication & HA
---

# Replication & HA

## WAL shipping

The recommended HA story for embedded deployments. Ships WAL segments to a replica directory or S3 staging area.

**Policies:**
- `OnCommit` — ship after every commit (RPO: 1 transaction)
- `Interval(d)` — ship every `d` seconds (RPO: `d` seconds)
- `OnSegmentSeal` — ship when a segment fills (highest throughput)

## Automatic failover

Leader election via WAL shipper health checks. When the primary fails, a replica is promoted automatically.

## Raft consensus

For strong consistency: `RaftNode` / `RaftCluster` with TCP network transport. Leader election, log replication, safety via majority ack.

## Read scaling

`ReadRouter` distributes read traffic across replicas. Writes go to the primary; reads round-robin across replicas.
