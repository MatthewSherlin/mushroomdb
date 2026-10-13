/**
 * mushroomdb-client — TypeScript client for mushroomdb.
 *
 * Entry point re-exports everything callers need.
 */

export { MushroomClient, MushroomError } from "./client.js";
export type {
  AlgoDir,
  AlgoReport,
  BackupReport,
  CellValue,
  ClientOptions,
  CreateNodeResult,
  DegreeConfig,
  DegreeReport,
  EdgeHistoryEntry,
  EdgeHistoryResult,
  EdgeInfo,
  Explanation,
  FindSimilarOptions,
  FindSimilarResult,
  HealthReport,
  HistoryChange,
  HistoryEntry,
  IngestEdge,
  IngestOptions,
  IngestReport,
  IngestRequest,
  MetricsReport,
  MutationEvent,
  Neighborhood,
  NodeEdgesResult,
  NodeHistoryResult,
  NodeInfo,
  PageRankConfig,
  PageRankReport,
  PredicateKind,
  PredicateSummary,
  QueryOptions,
  QueryParams,
  QueryResult,
  RuleDef,
  RulePredicate,
  RuleStats,
  RuleSuggestion,
  SlowQueryEntry,
  Stats,
  SuggestReport,
  WasLinkedResult,
  WccConfig,
  WccReport,
} from "./client.js";
export type {
  DbEvent,
  SubscribeMessage,
} from "./types.js";
export type {
  SubscribeHandle,
  SubscribeOptions,
  WatchHandle,
  WatchOptions,
  WsConstructor,
  WsLike,
} from "./ws.js";
