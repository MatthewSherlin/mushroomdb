import { MushroomClient } from "../src/client.js";
import type {
  DegreeConfig,
  DegreeReport,
  PageRankConfig,
  PageRankReport,
  WccConfig,
  WccReport,
} from "../src/types.js";

const client = new MushroomClient("http://127.0.0.1:8080");
declare const pagerank: PageRankConfig;
declare const degree: DegreeConfig;
declare const wcc: WccConfig;
declare const broadUnion: PageRankConfig | WccConfig;
declare const optionalPagerank: PageRankConfig | undefined;
declare const shared: { edge_type?: string | null; budget_ms?: number };
declare const sharedUnion: { edge_type?: string | null } | { budget_ms?: number };

const pr: Promise<PageRankReport> = client.algo("pagerank", pagerank);
const components: Promise<WccReport> = client.algo("wcc", wcc);
const degrees: Promise<DegreeReport> = client.algo("degree", degree);
void [pr, components, degrees];
client.algo("pagerank");
client.algo("wcc", undefined);
client.algo("degree", shared);
client.algo("wcc", sharedUnion);
client.algo("wcc", { edge_type: null, budget_ms: 10 });
declare const sharedWeights: { weight_prop?: string | null; min_weight?: number | null };
declare const weightUnion: { weight_prop?: string | null } | { min_weight?: number | null };
client.algo("pagerank", sharedWeights);
client.algo("wcc", sharedWeights);
client.algo("degree", sharedWeights);
client.algo("wcc", weightUnion);
client.algo("pagerank", { weight_prop: "score", min_weight: 0.1 });
client.algo("wcc", { weight_prop: null, min_weight: null });
client.algo("degree", { direction: "out", weight_prop: "score", min_weight: 0.1 });

// Declared keys matter even when the current value sets only shared fields.
// @ts-expect-error PageRankConfig also declares PageRank-only options.
client.algo("wcc", pagerank);
// @ts-expect-error Degree has no PageRank damping/iteration options.
client.algo("degree", pagerank);
// @ts-expect-error WCC does not support degree direction.
client.algo("wcc", degree);
// @ts-expect-error The possible PageRank union branch still has extra keys.
client.algo("wcc", broadUnion);
// @ts-expect-error Optional arguments cannot erase PageRank-only keys.
client.algo("wcc", optionalPagerank);
// @ts-expect-error Fresh unknown keys are refused too.
client.algo("wcc", { damping: 0.5 });
