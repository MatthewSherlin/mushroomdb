/**
 * mushroomdb-client — TypeScript client for mushroomdb.
 *
 * Entry point re-exports everything callers need.
 */

export { MushroomClient, MushroomError } from "./client.js";
export type {
  AlgoDir,
  AlgoReport,
  CellValue,
  DegreeConfig,
  DegreeReport,
  IngestEdge,
  IngestOptions,
  IngestReport,
  IngestRequest,
  PageRankConfig,
  PageRankReport,
  QueryOptions,
  QueryParams,
  QueryResult,
  RuleStats,
  RuleSuggestion,
  Stats,
  SuggestReport,
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
  WsConstructor,
  WsLike,
} from "./ws.js";
