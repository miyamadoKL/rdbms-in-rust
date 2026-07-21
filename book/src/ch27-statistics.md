# 第27章 統計情報とCardinality Estimation

```console
minidb> CREATE TABLE orders (id BIGINT NOT NULL, status BIGINT NOT NULL, amount BIGINT NOT NULL);
CREATE TABLE
minidb> EXPLAIN SELECT id FROM orders WHERE status = 1;
QUERY PLAN
----------
Projection(id) rows=5
  └─ Filter(status = 1) rows=5
    └─ SeqScan(orders) rows=1000
(3 rows)
```

`orders`にはまだ1行も入っていません。
それでも`EXPLAIN`は`rows=1000`、`rows=5`という具体的な数を返してきます。
この数はどこから来たのでしょうか。

20行だけ挿入し、実際の分布を`ANALYZE`で調べさせてから、同じ`EXPLAIN`をもう一度実行してみます。

```console
minidb> INSERT INTO orders VALUES (0, 0, 0), (1, 0, 5), (2, 0, 10), (3, 0, 15), (4, 0, 20), \
  (5, 0, 25), (6, 0, 30), (7, 0, 35), (8, 0, 40), (9, 1, 45), (10, 0, 50), (11, 0, 55), \
  (12, 0, 60), (13, 0, 65), (14, 0, 70), (15, 0, 75), (16, 0, 80), (17, 0, 85), (18, 0, 90), (19, 1, 95);
INSERT 20
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN SELECT id FROM orders WHERE status = 1;
QUERY PLAN
----------
Projection(id) rows=2
  └─ Filter(status = 1) rows=2
    └─ SeqScan(orders) rows=20
(3 rows)
```

`status`列は20行のうち2行だけが`1`で、`ANALYZE`後の`rows=2`はその実データの分布をそのまま反映した見積もりです。
`ANALYZE`前の`rows=5`(全体の0.5%という当てずっぽうの既定値)とは違い、`SeqScan(orders)`自体の見積もりも`rows=1000`から`rows=20`(実際の行数)へ変わっています。
この章では、`rows=2`という数がどう計算されるのか、そしてその数がどこまで信じられるのかを扱います。

## 前章の限界: 「どちらが速いか」に答えられない

第26章のルールベース最適化は、`1 = 1`のような冗長な項を消し、`customers.id = 1`のような片側だけの条件を該当するテーブルまで運びました。
どちらの書き換えも、**書き換えの前後で常に同じか、それより少ない行数しか扱わない**という性質を持っていたからこそ、無条件に適用してよいと言えました。
`Predicate Pushdown`は常に得なので、迷う余地がありません。

第25章のアクセスパス選択は、事情が違います。
`customers`(50行)と`orders`(`m`行)を結合するとき、`orders.customer_id`に索引があれば`physical_plan::optimize`はIndex Nested Loop Joinを選びました。
この選択が常に得かどうかを実測したところ、答えは「データ次第」でした。

```console
$ cargo test --release --lib -- --ignored --nocapture index_nested_loop_join_is_not_always_faster
selective m= 32000  matches=    33  IndexNestedLoopJoin=5.116327ms  HashJoin=55.265092ms
dense     m= 32000  matches= 32000  IndexNestedLoopJoin=786.240948ms  HashJoin=48.669114ms
```

一致する行がごく一部にとどまる**選択的な結合**ではIndex Nested Loop Joinが10倍以上速く、`orders`のほぼ全行が一致する**密な結合**ではHash Joinの15倍以上遅くなります(第25章)。
`orders.customer_id`に索引があるという構文的な事実だけを見ている限り、この2つの場面は区別できません。
`physical_plan::optimize`(第22章、第25章)は、まさにその構文的な事実だけでIndex Nested Loop Joinを選ぶルールになっており、選択的か密かを一切見ていません。

区別に要るのは、「`orders.customer_id = <外側の値>`という条件が、実際に何行を返すか」という問いへの答えです。
この問いは、SQLの構文からは決まりません。
`orders`にどんな値がどれだけ分布しているかという、**実データについての情報**が要ります。
この章はその情報(統計情報)を集める部分を作り、次章(第28章)がその情報を使って初めて「どちらが速いか」に数値で答えます。

## 統計収集: 行数、NULL数、NDV、Min/Max、MCV、Histogram

統計情報の型は、新規作成する`src/statistics.rs`に置きます。
この型は、テーブルの行数と、列ごとの5種類の値を集めます。

```rust
pub struct TableStats {
    pub row_count: u64,
    pub columns: Vec<ColumnStats>,
}

pub struct ColumnStats {
    pub null_count: u64,
    pub distinct_count: u64,
    pub min: Option<Value>,
    pub max: Option<Value>,
    pub mcv: Vec<(Value, u64)>,
    pub histogram: Vec<Bucket>,
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod statistics;
```

`mcv`(MCV、Most Common Values)は、出現回数の多い値を個別に(値そのものと実際の頻度の組で)保持するリストです。
これが要る理由は、等頻度Histogramだけでは特定の値に行が集中する分布をうまく扱えないことにあります(後述の「Histogramは等頻度(equi-depth)を選ぶ」を参照)。

`min`、`max`、`histogram`の対象は、[`crate::types::compare_values`](第21章の`ORDER BY`、`MIN`/`MAX`が使う全順序)で比較できる列すべてです。
`BOOLEAN`、`BIGINT`、`TEXT`のいずれも、この全順序の上で最小値、最大値、バケツ境界を持てます。

集計は、`src/statistics.rs`に定義する`StatsCollector`という1つの構造体が担います。

```rust
pub struct StatsCollector {
    row_count: u64,
    columns: Vec<ColumnAccumulator>,
}

impl StatsCollector {
    pub fn new(schema: &Schema) -> Self { /* ... */ }
    pub fn add_row(&mut self, tuple: &Tuple) { /* ... */ }
    pub fn finish(self) -> TableStats { /* ... */ }
}
```

`add_row`を1行ずつ呼び、最後に`finish`を呼ぶという形にしてあるのは、テーブルを`Vec<Tuple>`へ丸ごと読み込んでから集計するのではなく、`SeqScan`相当の走査(後述の`ANALYZE`)が返す行を1件ずつ流し込めるようにするためです。

### Distinct値数はHyperLogLogを使わず、`HashSet`で正確に数える

`distinct_count`(NDV、Number of Distinct Values)は、大規模なテーブルでは`HashSet`のような厳密な方法では収まらないことがよく知られています。
値の種類が数百万を超えるテーブルでは、行1件ごとに`HashSet`へ挿入する処理も、`HashSet`自体が使うメモリも無視できなくなり、実務のRDBMSの多くはHyperLogLogのような近似アルゴリズム(数KBのメモリで誤差数%のNDVを見積もる)を採用しています。

この章はその近似を採らず、`src/statistics.rs`で`HashSet`による厳密なカウントを行います。

```rust
struct ColumnAccumulator {
    null_count: u64,
    distinct: HashSet<Value>,
    // ...
}
```

理由は2つあります。
1つは、この章の目的が「Cardinality Estimationの推定式そのものが正しく機能するか」を確かめることにあり、そこへHyperLogLogの推定誤差というもう1つの不確実性を持ち込みたくないことです。
推定式を検証している最中に結果がずれても、それが推定式自体の問題なのか、入力に使ったNDVの近似誤差なのかを切り分けられなくなります。
もう1つは、HyperLogLogがハッシュ関数、レジスタ配列、調和平均による推定式を要する、この教材の1章に見合わない複雑さを持つことです。
`HashSet`によるNDVの厳密な計算は、大規模データに対してはスケールしないという限界を明確に持ちながらも、実装は数行に収まり、推定式の検証という当面の目的には十分です。
近似アルゴリズムへの置き換えは、章末の演習課題に譲ります。

### Histogramは等頻度(equi-depth)を選ぶ

`src/statistics.rs`に置くHistogramは、列の値の分布を`HISTOGRAM_BUCKET_COUNT`(固定で10)個のバケツに区切って持ちます。

```rust
pub const HISTOGRAM_BUCKET_COUNT: usize = 10;

pub struct Bucket {
    pub lower: Value,
    pub upper: Value,
    pub row_count: u64,
}
```

バケツの境界の決め方には、大きく2通りあります。
**等幅(equi-width)**は値の取りうる範囲を等分し、各バケツの区間の幅を揃えます。
**等頻度(equi-depth)**は逆に、各バケツに収まる行数をできるだけ揃え、区間の幅はバケツごとに変わります。

この章は等頻度を選びます。
理由は、一部の値に行が集中する歪んだ分布での精度です。
`status`列のように、1000行のうち900行が`0`、残り100行が1〜9のどれかというデータを考えます。
値の範囲(`0`〜`9`)を等幅に10分割すれば、`0`だけのバケツに90行、残り9個のバケツに1行ずつという、ほとんど意味のないHistogramになります。
等頻度なら、行数を10個のバケツへ均等に割り振るぶん、この極端な偏りは避けられます。
PostgreSQLの`ANALYZE`が作る`pg_stats.histogram_bounds`も、同じ理由で等頻度方式を採っています。

もっとも、等頻度そのものにも弱点が残ります。
1つの値だけで1バケツぶんの目標行数を超えてしまう場合です。
`status`列と同じ、1000行のうち900行が`0`という分布を、`src/statistics.rs`のテストで確かめます(サンプルを99行に縮めています)。

```rust
#[test]
fn skewed_distribution_moves_the_dominant_value_into_mcv() {
    // 0が90回、1〜9がそれぞれ1回ずつ出現する、値に偏りのある分布。
    let mut values = vec![Value::BigInt(0); 90];
    values.extend((1..10).map(Value::BigInt));
    let stats = collect(&values);
    let column = &stats.columns[0];

    // 平均バケツ行数は99/10=9.9。0の出現回数(90)はこれを大きく超えるため
    // MCVに採用され、Histogramには残余の9値(1〜9、各1回)だけが残る。
    assert_eq!(column.mcv, vec![(Value::BigInt(0), 90)]);
    let residual_total: u64 = column.histogram.iter().map(|b| b.row_count).sum();
    assert_eq!(residual_total, 9);
}
```

1バケツあたりの平均行数(99行÷10バケツ≒9.9行)を、値`0`(90行)が大きく超えています。
`build_equi_depth_histogram`が行数だけを見て機械的に等頻度分割すると、この90行は複数のバケツにまたがります。
`col = 0`の等値述語は、値`0`を含む**先頭の1バケツだけ**を見て見積もる仕組みなので、90行のうち一部しか数えられていないバケツの行数比率から見積もることになり、実際の90行よりはるかに小さい値を返してしまいます。

この問題を避けるため、Histogramを組み立てる前に、平均バケツ行数を上回る頻度を持つ値を**MCV**(Most Common Values、最頻値)として個別に抜き出し、Histogramはそれを除いた残りの値だけから組み立てます。
PostgreSQLの`pg_stats.most_common_vals`と同じ役割分担です。

ただし、「抽出前の全体行数から一度だけ閾値を計算する」だけでは、この問題を防ぎきれません。
100行が`0`(50行)、`1`(8行)、`2`〜`43`(各1行)という分布を考えます。

抽出前の平均バケツ行数(100行÷10バケツ=10行)だけを見ると、`1`(8行)はこの10行を超えないためMCVに採用されません。
`0`(50行)をMCVへ移したあとの残り50行は、まだ10個のバケツに分割されます(値の種類は`1`〜`43`の43種類あるため)。
1バケツあたり5行という、新しい平均バケツ行数のもとでは、`1`(8行)はこの5行を超えているにもかかわらず、抽出前の(古い)閾値だけで判定するとMCVに移らないまま残ります。
その結果、`1`の8行は単一値バケツ(5行)と隣接する混合バケツ(3行)に分割され、`v = 1`の見積もりは先頭のバケツの5行しか数えられません。

この問題を避けるため、MCVは**固定点**まで抽出します。
値を1つ抽出するたびに、まだMCVへ移していない残りの行数から平均バケツ行数を**再計算**し、その新しい閾値を上回る値がもう無くなるまで繰り返す`extract_mcv`を、`src/statistics.rs`に定義します。

```rust
fn extract_mcv(sorted_values: &[Value]) -> (Vec<(Value, u64)>, Vec<Value>) {
    if sorted_values.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // ソート済みなので、同じ値は連続して並ぶ。連続run(同じ値の連なり)を
    // 数えるだけで、値ごとの出現回数が求まる。
    let mut counts: Vec<(Value, u64)> = Vec::new();
    for value in sorted_values {
        match counts.last_mut() {
            Some((last_value, count)) if last_value == value => *count += 1,
            _ => counts.push((value.clone(), 1)),
        }
    }
    // 出現回数の降順。同数なら値の昇順(`counts`はソート済みの値順)で決定的に揃える。
    counts.sort_by(|(value_a, count_a), (value_b, count_b)| count_b.cmp(count_a).then_with(|| compare_values(value_a, value_b)));

    // 出現回数の多い値から順に、「その時点で残っている非NULL行数」に対する
    // 平均バケツ行数(`build_equi_depth_histogram`が実際に作るバケツ数
    // `min(残り行数, HISTOGRAM_BUCKET_COUNT)`で割った値)を都度再計算しながら
    // 抽出する。`counts`は降順なので、ある値がその時点の閾値を超えなければ、
    // それ以降の値(出現回数がさらに少ない)も、これ以降の閾値(残り行数の
    // 減少に伴って単調非増加)を超えることは無い。したがって、超えない値に
    // 出会った時点で走査を打ち切ってよい(固定点に達したことを意味する)。
    let mut mcv: Vec<(Value, u64)> = Vec::new();
    let mut remaining_rows = sorted_values.len() as u64;
    for (value, count) in counts {
        if mcv.len() >= MCV_MAX_ENTRIES {
            break;
        }
        let effective_bucket_count = remaining_rows.min(HISTOGRAM_BUCKET_COUNT as u64).max(1);
        let average_bucket_size = remaining_rows as f64 / effective_bucket_count as f64;
        if count as f64 > average_bucket_size {
            remaining_rows = remaining_rows.saturating_sub(count);
            mcv.push((value, count));
        } else {
            break;
        }
    }

    if mcv.is_empty() {
        return (Vec::new(), sorted_values.to_vec());
    }

    let mcv_values: HashSet<&Value> = mcv.iter().map(|(value, _)| value).collect();
    let residual: Vec<Value> = sorted_values.iter().filter(|value| !mcv_values.contains(value)).cloned().collect();
    (mcv, residual)
}
```

`0`(50行)を抽出した時点で残り行数は50、平均バケツ行数は5に下がります。
`1`(8行)はこの新しい閾値(5行)を上回るため、続けてMCVへ移ります。
残った`2`〜`43`(各1行、合計42行)は、この閾値(次の残り行数42に対する平均4.2行)を上回らないため、ここで抽出が止まります。
`v = 1`の等値述語は、この`1`がMCVに載ったことでその実際の頻度(8/100)をそのまま返します。

```console
minidb> -- vが0(50行)、1(8行)、2〜43(各1行、合計100行)というテーブルをANALYZE
minidb> ANALYZE t;
ANALYZE 1
minidb> EXPLAIN ANALYZE SELECT v FROM t WHERE v = 1;
QUERY PLAN
----------
Projection(v) rows=8 actual=8
  └─ Filter(v = 1) rows=8 actual=8
    └─ SeqScan(t) rows=100 actual=100
(3 rows)
```

「値を1つ抽出するたびに平均バケツ行数を再計算する」という設計上、[`MCV_MAX_ENTRIES`](10)件に達する前に、残りのどの値も新しい閾値を超えなくなる(=固定点に達する)のが通常です。
それでも、極端に段階的な分布(出現回数が少しずつ減っていく多数の値)では、10件に達してもまだ固定点に届かない場合があります。
この場合は抽出をそこで打ち切り、MCVの閾値をわずかに下回る値が残余に残ります。

### Histogramは同値の連続runをバケツ境界で分割しない

残余に残ったこの手の値は、まだ1つのバケツの目標行数は上回っています。
バケツの組み立てを「ソート済みの残余値を`行数 ÷ バケツ数`ぶんずつ機械的に切り分ける」だけの単純な実装にしていると、この値がちょうど切れ目をまたいでしまい、**単一値バケツ**(その値だけで埋まったバケツ)と**混合バケツ**(その値の残りと、別の値が混在するバケツ)に分割されてしまいます。

出現回数`[15, 14, 12, 11, 10, 9, 8, 7, 7, 6, 6]`(値`0`〜`10`)に、一意な値42件(`1000`〜`1041`)を加えた147行のテーブルで確かめます。

```console
minidb> -- vが0〜10(出現回数[15,14,12,11,10,9,8,7,7,6,6])と、
minidb> -- 1000〜1041(一意な値、各1行)からなる147行のテーブルをANALYZE
minidb> ANALYZE t;
ANALYZE 1
minidb> EXPLAIN ANALYZE SELECT v FROM t WHERE v = 10;
QUERY PLAN
----------
Projection(v) rows=6 actual=6
  └─ Filter(v = 10) rows=6 actual=6
    └─ SeqScan(t) rows=147 actual=147
(3 rows)
```

`0`〜`9`(10個)は、MCVの固定点抽出([`MCV_MAX_ENTRIES`]、10件)にちょうど達するまでに移り、`10`(出現回数6)はこの上限によって残余に残ります。
`10`を一意な値(`1000`〜`1041`)よりすべて小さくしてあるため、ソート順で`10`の6行はまとまって先頭に並びます。
残余は48行(`10`が6行、一意な値が42行)、equi-depthの目標バケツ行数は48÷10=4.8行(切り捨てて4行、端数8個は先頭のバケツへ1行ずつ多め)です。
行数だけを見て機械的に切り分けると、1個目のバケツ(目標5行)は`10`の5行、2個目のバケツ(目標5行)は`10`の残り1行と一意な値4件、という具合に、`10`の6行がバケツをまたいでしまいます。
`col = 10`の等値述語は、値`10`を含む**先頭の1バケツだけ**を見て見積もる仕組みなので、6行のうち一部しか数えられていないバケツの行数比率から見積もることになり、実際の6行よりはるかに小さい値を返してしまいます。

この問題を避けるため、`src/statistics.rs`の`build_equi_depth_histogram`は、目標の切れ目が同じ値の連続run(同じ値が連なった範囲)の途中に来る場合、runの終わりまで境界を伸ばします。

```rust
fn build_equi_depth_histogram(sorted_values: &[Value]) -> Vec<Bucket> {
    if sorted_values.is_empty() {
        return Vec::new();
    }

    let total = sorted_values.len();
    let bucket_count = total.min(HISTOGRAM_BUCKET_COUNT);
    let base_size = total / bucket_count;
    let remainder = total % bucket_count;

    let mut buckets = Vec::new();
    let mut start = 0;
    let mut bucket_index = 0;
    while start < total {
        // 割り切れない分は、先頭のバケツから1行ずつ多めに配る、という目標
        // サイズ自体は従来どおり。
        let target_size = base_size + usize::from(bucket_index < remainder);
        let mut end = (start + target_size.max(1)).min(total);
        // 目標の切れ目が同じ値の連続run(同じ値が連なった範囲)の途中に
        // 来る場合、runの終わりまで境界を伸ばす。これにより、1つの値が
        // 2つのバケツにまたがることはなくなる(モジュール冒頭の説明を参照)。
        // 結果としてバケツの行数は均等ではなくなる(runが長い値のぶん、
        // そのバケツだけ目標サイズを超える)。
        while end < total && sorted_values[end] == sorted_values[end - 1] {
            end += 1;
        }
        let chunk = &sorted_values[start..end];
        buckets.push(Bucket {
            lower: chunk.first().expect("startを含むため1行以上").clone(),
            upper: chunk.last().expect("startを含むため1行以上").clone(),
            row_count: chunk.len() as u64,
        });
        start = end;
        bucket_index += 1;
    }
    buckets
}
```

先ほどの147行の例では、1個目のバケツの目標の切れ目(5行目)がちょうど`10`の連続runの途中(6行のうち5行目)に来ます。
境界をrunの終わりまで伸ばすことで、`10`の6行はすべて1個目のバケツに収まります(`lower == upper == 10`、`row_count = 6`)。
`col = 10`はこのバケツ1個だけを見れば正確な行数(6行)が分かるため、`rows=6`が`actual=6`と一致します。

この設計の代わりに、バケツの行数は均等ではなくなります。
目標の切れ目をまたぐ長いrunがあるバケツは、目標行数を超えて膨らみます(runの長さがそのままそのバケツの行数になります)。
これは等頻度(equi-depth)という名前が示す「バケツの行数を均等にする」という理想からの意図的な後退ですが、行数の均等さそのものより「値がバケツをまたがない」ことのほうが、この章の推定式にとって重要です。
値がバケツをまたがなくなったことで、等値述語の推定(`equality_selectivity_within_non_null`、次節)は「値を含むバケツ」をちょうど1個だけ見ればよくなり、複数のバケツにまたがった分を合算するような処理は不要になります。
永続化した統計を検証する`validate_stats_metadata`(後述)も、バケツの行数がおおむね均等であることは前提にしていません(バケツ数の上限、各バケツの`row_count > 0`、境界の昇順、行数合計の一致だけを検査します)。

「値を含むバケツはちょうど1個」という不変条件は、実装(`build_equi_depth_histogram`)がその場で保つだけでなく、`validate_stats_metadata`が**永続化された統計に対しても**検査します。
隣接するバケツの境界は、単なる昇順(`前のバケツのupper <= 次のバケツのlower`)ではなく、`前のバケツのupper < 次のバケツのlower`という厳密な分離を要求します。
`<=`のままだと、値`1`を1行ずつ持つ同一境界(`lower == upper == 1`)の2バケツのような統計を受理してしまい、`v = 1`の選択率はどちらか一方のバケツの1行しか見ないまま、実際の2行の半分(期待値0.10に対して0.05)になってしまいます。
この検査を追加する前のブランチの開発途中(Histogramが同値の連続runをバケツ境界で分割していた時期のコミット)でだけ生成されえた、隣接バケツが境界を共有する形式の統計は、この検査により再オープン時に`DbError::CorruptCatalog`として決定的に拒否されます。
章をまたいだファイル互換性を約束しないという、この教材が一貫して採っている方針の範囲内の変更です。

## `ANALYZE`文: 統計を集める

`ANALYZE [テーブル名]`という新しい文を追加します。
テーブル名を省略すると、カタログに登録されている全テーブルが対象になります。

```console
minidb> ANALYZE orders;
ANALYZE 1
minidb> ANALYZE;
ANALYZE 3
```

構文解析(`src/lexer.rs`の`Keyword::Analyze`、`src/parser.rs`の`parse_analyze_statement`)は`CREATE TABLE`、`DROP TABLE`と同じくらい単純です。
束縛(`src/binder.rs`)も、`DropTable`と同じ方針を採りました。
テーブル名が指定されていれば、その存在だけをここで確認し(未知のテーブル名は位置情報付きの`DbError::Bind`になります)、ASTのバリアントをそのまま`BoundStatement::Analyze`として通します。
`ANALYZE`は既存のカタログエントリの統計欄を書き換えるだけの操作であり、`CREATE INDEX`、`DROP INDEX`(第24章)と同じく、式の名前解決や型検査を必要としないからです。

実行(`Database::execute_analyze`、`src/database.rs`)は、対象テーブルを`SeqScan`と同じ経路で1回走査します。

```rust
fn collect_table_stats(&self, table_id: TableId, table_name: &str, schema: &Schema) -> DbResult<TableStats> {
    let plan = PhysicalPlan::SeqScan(SeqScanNode {
        table_id,
        table_name: table_name.to_string(),
        schema: schema.clone(),
    });
    let mut executor = self.build_query_executor(&plan, None)?;
    let mut collector = StatsCollector::new(schema);
    while let Some(tuple) = executor.next()? {
        collector.add_row(&tuple);
    }
    Ok(collector.finish())
}
```

`PhysicalPlan::SeqScan`を直接組み立て、`build_query_executor`(第19章から`SELECT`が使っているのと同じ関数)へ渡しているだけです。
`Backend::Memory`、`Backend::Disk`のどちらであっても、`SeqScan`の実行(`MemSeqScanExec`、`DiskSeqScanExec`)がすでにこの違いを吸収しているため、`ANALYZE`の側でバックエンドを意識する必要はありません。

## 統計の保存とデフォルト選択率

`src/database.rs`の`Backend::Memory`の統計は、`Catalog`、`MemStorage`と同じくプロセスのメモリ上だけに保持し、永続化しません。

```rust
enum Backend {
    Memory {
        catalog: Catalog,
        storage: MemStorage,
        stats: HashMap<TableId, TableStats>,
    },
    Disk {
        storage: Box<Storage>,
    },
}
```

`Backend::Disk`は、`src/storage.rs`のCatalogページへ統計情報を追記します。
索引メタデータ(第24章)が末尾にセクションを追加したのと同じパターンで、既存のレイアウトの末尾に`stats`セクションを追記するだけです。

```text
stats_count: u32
stats × stats_count:
    table_id:      u64
    row_count:     u64
    column_count:  u16
    columns × column_count:
        null_count:     u64
        distinct_count: u64
        min:            Value
        max:            Value
        mcv_count:      u16
        mcv × mcv_count:
            value: Value
            count: u64
        bucket_count:   u16
        buckets × bucket_count:
            lower:     Value
            upper:     Value
            row_count: u64
```

`Value`(`NULL`/`BOOLEAN`/`BIGINT`/`TEXT`)は、既存の`data_type_to_u8`(列の**型**だけを表す1バイト)とは別に、`src/storage.rs`の`encode_value`が値そのものを復元できる自己記述形式でエンコードします。

```rust
fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0),
        Value::Boolean(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::BigInt(n) => {
            out.push(2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(3);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}
```

`min`と`max`は`Option<Value>`ですが、`StatsCollector`はそもそも`NULL`値を`min`/`max`の対象に含めないため、「値が1件も無い」(`None`)と`Value::Null`が同時に起こることはありません。
この不変条件を使い、`None`を`Value::Null`と同じタグ(`0`)で表すことで、存在を示す専用のフラグバイトを別に持たずに済ませています。

このセクションを追加することに伴い、`CATALOG_LAYOUT_VERSION`を`1`から`2`へ上げます。
モジュール冒頭のドキュメントコメント(第24章までの節と同じ形式)にも、この変更を書き残してあります。
章をまたいだファイル互換性を約束しない方針(第15章から一貫)はそのままで、`CATALOG_MAGIC`と`CATALOG_LAYOUT_VERSION`による判定が、レイアウトの変わった古いカタログを確実に拒否します(`mcv`セクションを列の途中へ挿入した第4部レビュー対応で、`CATALOG_LAYOUT_VERSION`はさらに`2`から`3`へ上がっています)。

構文的にバイト列を復元できることと、その中身が意味をなすことは別の話です。
`decode_catalog`が検査するのは統計に紐づく`TableId`の重複だけで、対応するテーブル定義との整合性(列数や型が一致するか、`null_count`が`row_count`を超えていないか、バケツの境界が昇順に並んでいるかなど)までは見ていませんでした。
これは、`Storage::open`が復元したカタログをそのまま信用してしまう欠落であり、破損したファイルや`Storage::set_table_stats`への不正な入力を、意味の壊れた統計情報のまま受理してしまいます。
第4部レビュー対応では、この意味検証をまとめて行う`validate_stats_metadata`を追加し、`Storage::open`(カタログを復元する経路、違反は`DbError::CorruptCatalog`)と`Storage::set_table_stats`(`ANALYZE`が新しい統計を登録する経路、違反は`DbError::InvalidStats`)の両方から呼んでいます。
検査項目(列数と型の一致、`null_count <= row_count`、`distinct_count <= 非NULL行数`、MCVとバケツそれぞれの上限件数、バケツの境界順序、MCVの値やバケツ境界がMin/Maxの範囲や型に収まること、MCVとバケツの行数合計が非NULL行数に一致すること)の詳細は`src/storage.rs`の`validate_stats_metadata`のドキュメントコメントを参照してください。

`ANALYZE`を一度も実行していないテーブルは、統計を持ちません。
この場合の推定は、新規作成する`src/estimator.rs`に置く選択率の慣用定数にフォールバックします。

```rust
/// 等値述語のデフォルト選択率(統計が無い場合)。出典は本文を参照。
pub const DEFAULT_EQ_SEL: f64 = 0.005;

/// 不等号述語のデフォルト選択率(統計が無い場合)。出典は本文を参照。
pub const DEFAULT_INEQ_SEL: f64 = 1.0 / 3.0;
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod estimator;
```

この2つの値は、PostgreSQLの`selfuncs.c`が同じ役割で使っている定数(`DEFAULT_EQ_SEL`と`DEFAULT_INEQ_SEL`)から取っています。
`DEFAULT_EQ_SEL`(0.5%)は、「典型的なテーブルでは、ある列がある1つの値と一致する行はおよそ200行に1行だろう」という経験則です。
`DEFAULT_INEQ_SEL`(1/3)は、「範囲述語(`<`や`>`)は、値の分布に関する手がかりが無くても、全体のおよそ3分の1を切り出すだろう」という経験則です。
どちらも統計的に導出された値ではなく、実際のワークロードで長年使われてきた経験則としての妥当性から採用されています。
章の冒頭で見た`rows=5`(`ANALYZE`前の`WHERE status = 1`、1000行のテーブルに対して)は、まさにこの`DEFAULT_EQ_SEL`(0.005)を1000に掛けた値です。

## Cardinality Estimation: 推定式

推定式は`src/estimator.rs`に集めます。
どの式も、`Option<&ColumnStats>`(統計が無ければ`None`)と対象テーブルの行数(`row_count`)を受け取り、選択率(0.0〜1.0の`f64`)または行数を返す、単純な関数です。

選択率が指すのは、一貫して「**対象範囲の全行(`NULL`を含む)**のうち、述語が`TRUE`と評価される行の割合」です。
SQLは`TRUE`、`FALSE`、`UNKNOWN`の3値論理を使い、`NULL`を含む比較は`UNKNOWN`になります。
`WHERE`句は`UNKNOWN`の行を`FALSE`と同じく落とすため、選択率は「非NULL行のうち一致する割合」ではなく「全行のうちTRUEになる割合」でなければなりません。
以下の各推定式は、`null_count`と`row_count`から求めた**NULL率**を使ってこれを見積もります。

### 等値述語: `col = 定数`

まず、値が`NULL`(`col = NULL`のような式)であれば、この比較は列の値によらず常に`UNKNOWN`になるため、選択率は0.0です(`v = NULL`は決して`TRUE`になりません)。
それ以外は、`src/estimator.rs`の`estimate_equality_selectivity`が、「列が非NULLである割合」と「非NULL行の中での一致割合」の積で見積もります。

```rust
pub fn estimate_equality_selectivity(stats: Option<&ColumnStats>, row_count: u64, value: &Value) -> f64 {
    if value.is_null() {
        return 0.0;
    }
    let Some(stats) = stats else { return DEFAULT_EQ_SEL };
    let non_null = 1.0 - null_fraction(stats.null_count, row_count);
    non_null * equality_selectivity_within_non_null(stats, row_count, value)
}
```

「非NULL行の中での一致割合」は、`src/estimator.rs`の`equality_selectivity_within_non_null`が、MCV(最頻値)に定数が載っていればその実頻度を、無ければ残余のHistogram(バケツの行数比率を、バケツ内のDistinct値数で割った近似)を、Histogramも無ければ`1 / NDV`を、NDVも無ければ`DEFAULT_EQ_SEL`を使う4段階のフォールバックで返します。

```rust
fn equality_selectivity_within_non_null(stats: &ColumnStats, row_count: u64, value: &Value) -> f64 {
    let non_null_rows = (row_count.saturating_sub(stats.null_count)) as f64;
    if non_null_rows <= 0.0 {
        return DEFAULT_EQ_SEL;
    }

    // MCV(最頻値)に載っている値は、Histogramより先に実頻度で答える
    // (`crate::statistics`モジュールの説明を参照)。
    if let Some(&(_, count)) = stats.mcv.iter().find(|(v, _)| v == value) {
        return (count as f64 / non_null_rows).clamp(0.0, 1.0);
    }

    if !stats.histogram.is_empty() {
        // Histogramは、MCVに載った値を除いた残余の非NULL値だけで組み立てて
        // ある(`crate::statistics::StatsCollector::finish`)。残余のNDVは
        // 全体のNDVからMCVの件数を引いたものである。
        let residual_ndv = (stats.distinct_count.saturating_sub(stats.mcv.len() as u64)).max(1) as f64;
        let bucket_count = stats.histogram.len() as f64;
        let ndv_per_bucket = (residual_ndv / bucket_count).max(1.0);
        for bucket in &stats.histogram {
            if compare_values(value, &bucket.lower) != Ordering::Less && compare_values(value, &bucket.upper) != Ordering::Greater
            {
                // `build_equi_depth_histogram`(`crate::statistics`)は、同じ値の
                // 連続runをバケツ境界で分割しない。したがって`value`を含む
                // バケツは必ずちょうど1個であり、複数のバケツにまたがって
                // 合算する必要は無い。
                if compare_values(&bucket.lower, &bucket.upper) == Ordering::Equal {
                    // このバケツの中身は`value`だけ(単一値バケツ)なので、
                    // 実際の行数をそのまま使える。
                    return (bucket.row_count as f64 / non_null_rows).clamp(0.0, 1.0);
                }
                // 混合バケツ(複数のDistinct値が入っている)。バケツ内の行が
                // 均等にDistinct値へ散らばっているとみなし、1つの値あたりの
                // 行数を求める。分母は非NULL行全体(残余だけではない)なので、
                // ここで直接「非NULL行の中での割合」になる。
                return (bucket.row_count as f64 / ndv_per_bucket / non_null_rows).clamp(0.0, 1.0);
            }
        }
        // どのバケツにも収まらない(観測範囲の外の値)。稀な値として扱う。
        return DEFAULT_EQ_SEL;
    }

    if !stats.mcv.is_empty() {
        // Histogramは空だが、MCVは非空。`extract_mcv`はHistogramに残余の
        // 値が1つでも残っていれば必ず1個以上のバケツを作る
        // (`crate::statistics::build_equi_depth_histogram`)ため、Histogramが
        // 空ということは残余が空、つまりMCVが非NULLの値をすべて網羅して
        // いることを意味する。`value`はここまでにMCVへ見つからなかった
        // (このifより前の`if let Some(...)`を参照)ので、`value`はこの列に
        // 一度も出現していないと確定できる(第4部2巡目レビュー対応、
        // codexの再現: `a`が常に`0`の列に対する`a = 1`が、統計上は稀では
        // なく確実に0行のはずが、次の`1 / distinct_count`という単純な
        // NDVフォールバックのせいで実際には無視できない値を返していた)。
        return 0.0;
    }

    // MCVもHistogramも無い(Distinct値数しか分からない)場合だけ、
    // 「NDV個の値が一様に分布している」という最も粗い仮定にフォールバックする。
    if stats.distinct_count > 0 {
        1.0 / stats.distinct_count as f64
    } else {
        DEFAULT_EQ_SEL
    }
}
```

冒頭の例(`status`列、20行中2行が`1`)がまさにこの経路を通ります。
平均バケツ行数(20行÷10バケツ=2行)を、値`1`の出現回数(2回)は上回らないため、MCVには採用されません。
残余のequi-depth Histogramは、同値の連続runをバケツ境界で分割しないため、この2行を1個の単一値バケツにまとめます。
`lower == upper == value`という条件でこのバケツを検出し、その実際の行数(2行)をそのまま使うことで、`col = 1`の選択率は2/20(=`rows=2`)を正しく返します。

`0`(90行)や`0`(50行)のように、1つの値だけでMCVの閾値を超え、その値以外の非NULL値が1つも残らない列(`a`が常に`0`固定の列など)では、残余のHistogramは空になります。
MCVは「Histogramが空になった」時点で、この列の非NULLの値をすべて網羅しています。
したがって、問い合わせた値がそのMCVに無ければ、その値はこの列に一度も出現していないと確定できます(0.0)。
`stats.distinct_count > 0`のときの`1 / distinct_count`というフォールバックは、MCVもHistogramも無い(NDVしか分からない)場合専用であり、MCVが非NULL値を網羅している場合に使うと、確実に0行のはずの値へ無視できない選択率を与えてしまいます(第4部2巡目レビュー対応)。

### 範囲述語: `col > / >= / < / <= 定数`

等値述語と同じ理由で、値が`NULL`なら選択率は0.0です。
それ以外は「列が非NULLである割合」×「非NULL行の中での選択率」の積で見積もります。
「非NULL行の中での選択率」は、各バケツについて「そのバケツの両端がどちらも述語を満たすか」を見ます。
両端とも満たせばバケツ全体を選択率1.0として数え、両端とも満たさなければ0.0です。
片方だけ満たす(バケツの内部に境界がある)場合、`BIGINT`なら`src/estimator.rs`の`bucket_overlap_fraction`が`(value - lower) / (upper - lower)`という線形補間(区間内での`value`の位置の比率)で按分します。

`src/estimator.rs`に次の`RangeOp`を定義します。

```rust
pub enum RangeOp {
    Gt,
    Ge,
    Lt,
    Le,
}
```

```rust
fn bucket_overlap_fraction(op: RangeOp, value: &Value, lower: &Value, upper: &Value) -> f64 {
    let satisfies = |x: &Value| -> bool {
        let ord = compare_values(x, value);
        match op {
            RangeOp::Gt => ord == Ordering::Greater,
            RangeOp::Ge => ord != Ordering::Less,
            RangeOp::Lt => ord == Ordering::Less,
            RangeOp::Le => ord != Ordering::Greater,
        }
    };
    match (satisfies(lower), satisfies(upper)) {
        (true, true) => 1.0,
        (false, false) => 0.0,
        _ => {
            // バケツの内部に境界がある。`value`がバケツのどの位置にあるかを、
            // 「lowerからvalueまでの距離」÷「lowerからupperまでの距離」の
            // 割合として求め、満たす側(`satisfies(upper)`かどうか)に応じて
            // その割合か、その補数を返す。
            let position = linear_interpolation_position(value, lower, upper);
            match position {
                Some(position) => {
                    if satisfies(upper) {
                        // upper側が満たす: valueより上の部分が対象。
                        (1.0 - position).clamp(0.0, 1.0)
                    } else {
                        // lower側が満たす: valueより下の部分が対象。
                        position.clamp(0.0, 1.0)
                    }
                }
                None => 0.5,
            }
        }
    }
}
```

線形補間の位置計算は、`src/estimator.rs`の`linear_interpolation_position`に切り出しています。

```rust
fn linear_interpolation_position(value: &Value, lower: &Value, upper: &Value) -> Option<f64> {
    match (value, lower, upper) {
        (Value::BigInt(v), Value::BigInt(lo), Value::BigInt(hi)) if hi != lo => {
            let v = i128::from(*v);
            let lo = i128::from(*lo);
            let hi = i128::from(*hi);
            let position = (v - lo) as f64 / (hi - lo) as f64;
            Some(position.clamp(0.0, 1.0))
        }
        _ => None,
    }
}
```

`linear_interpolation_position`は`BIGINT`どうしの`lower`、`upper`、`value`に対してだけ`Some((value - lower) / (upper - lower))`を返します。
`TEXT`と`BOOLEAN`は`compare_values`による大小比較(全順序)は持ちますが、2値の「距離」(差)を定義する演算を持ちません。
`"apple"`と`"banana"`の間に`"apricot"`がどれだけ近いかを測る自然な数値は存在しないため、これらの型は`None`(中点0.5という一様分布の仮定)にとどめます。

`v - lo`と`hi - lo`を`i64`のまま引き算すると、`lower`が`i64::MIN`に近い側、`upper`が大きい側にある区間では、有効な`i64`どうしの差でも`i64`の表現範囲(最大で`u64::MAX`、2^64-1に達しうる)を超え、デバッグビルドでは`attempt to subtract with overflow`で停止してしまいます(第4部2巡目レビュー対応)。
そのため、`v`、`lo`、`hi`を一旦`i128`へ拡張してから引き算し、その結果を`f64`へ変換します。

Histogramのバケツごとに`bucket_overlap_fraction`を呼び、行数で重み付けして合計します。
MCVは実際の値そのものを持つため、バケツのような近似は要りません。
各MCVの値を直接`op`で判定し(`satisfies_range`)、満たす値の出現回数をそのまま合計に加えます。
MCVもHistogramも無い(NDVやMin/Maxしか分からない)場合だけ、Min/Maxを1個のバケツとみなして`bucket_overlap_fraction`を呼ぶフォールバックを使い、Min/Maxも無ければ`DEFAULT_INEQ_SEL`です。

### `IS NULL` / `IS NOT NULL`

`IS NULL`の選択率は、全行のうち`NULL`である割合、つまり`null_count / row_count`(**NULL率**)そのものであり、`src/estimator.rs`の`null_fraction`がこれを計算します。

```rust
pub fn null_fraction(null_count: u64, row_count: u64) -> f64 {
    if row_count == 0 {
        return 0.0;
    }
    (null_count as f64 / row_count as f64).clamp(0.0, 1.0)
}
```

`null_fraction`は、前述の`estimate_equality_selectivity`や`estimate_range_selectivity`が「列が非NULLである割合」を求めるためにも使う、共通の関数です。
同じ`src/estimator.rs`に、これを使う`estimate_is_null_selectivity`を続けて定義します。

```rust
pub fn estimate_is_null_selectivity(stats: Option<&ColumnStats>, row_count: u64) -> f64 {
    match stats {
        Some(stats) => null_fraction(stats.null_count, row_count),
        None => DEFAULT_EQ_SEL,
    }
}
```

`IS NULL`と`IS NOT NULL`は(`UNKNOWN`を経由せず)全行をちょうど2つに分けるため、`IS NOT NULL`の選択率は単純な補数`1 - IS NULLの選択率`で正確に求まり、`src/estimator.rs`の`estimate_is_not_null_selectivity`がこれをそのまま実装します。

```rust
pub fn estimate_is_not_null_selectivity(stats: Option<&ColumnStats>, row_count: u64) -> f64 {
    1.0 - estimate_is_null_selectivity(stats, row_count)
}
```

### `AND` / `OR` / `NOT`

`AND`は独立性を仮定した積、`OR`は包除原理で求まりそうに見え、`NOT`は「1引く元の値」で求まりそうに見えます。
ところが、この単純な式は3値論理の確定規則と食い違います。

100行すべてで`a`が`0`固定(`a = 1`は常に`FALSE`)、`b`が`NULL`固定(`b = 1`は常に`UNKNOWN`)というテーブルで確かめます。

```console
minidb> -- aが常に0、bが常にNULLの100行をANALYZE
minidb> ANALYZE tri;
ANALYZE 1
minidb> EXPLAIN ANALYZE SELECT a FROM tri WHERE NOT (a = 1 AND b = 1);
QUERY PLAN
----------
Projection(a) rows=100 actual=100
  └─ Filter(NOT (a = 1 AND b = 1)) rows=100 actual=100
    └─ SeqScan(tri) rows=100 actual=100
(3 rows)
```

`a = 1`は常に`FALSE`なので、`b = 1`が`UNKNOWN`であっても内側の`a = 1 AND b = 1`は常に`FALSE`に確定します(`FALSE AND UNKNOWN`は`FALSE`という、3値論理の真理値表そのものです)。
`NOT`後は常に`TRUE`になるため、`actual=100`(全行)です。

もし選択率を「`TRUE`になる確率」の1値だけで扱うと、この確定規則を再現できません。
`sel(a = 1)`はほぼ0、`sel(b = 1)`は列`b`のNULL率(ほぼ1.0)に応じてほぼ0(比較の相手が`NULL`なら`UNKNOWN`、`WHERE`句は`UNKNOWN`を`TRUE`として拾わないため)になり、単純な積`sel(a=1) × sel(b=1)`はほぼ0のままです。
第1巡目のレビュー対応では、`NOT(p)`を「`p`の被演算子が`UNKNOWN`にならない割合(`known_fraction`)からの補数」として見積もっていましたが、`known_fraction`を`AND`の両辺の積(「両辺とも`UNKNOWN`でない割合」)で近似していたため、`b`のほぼ全行が`UNKNOWN`である以上`known_fraction`もほぼ0になり、`NOT`後の見積もりも実際(`rows=100`)よりはるかに小さい値になっていました。
問題は、`FALSE AND UNKNOWN`が`FALSE`(=`UNKNOWN`ではなく確定している)という規則を、「両辺とも非`UNKNOWN`か」という指標だけでは表せないことにあります。

これを正しく扱うには、選択率を`TRUE`の1値ではなく、**`TRUE`/`FALSE`/`UNKNOWN`の3確率**として持ち運ぶ必要があります(第4部2巡目レビュー対応)。
`src/estimator.rs`に次の`Selectivity3`を定義します。

```rust
pub struct Selectivity3 {
    pub is_true: f64,
    pub is_false: f64,
    pub is_unknown: f64,
}
```

比較や`IS [NOT] NULL`という葉では、この3確率をそれぞれの列のNULL率から組み立てます(`col <op> 定数`なら`is_unknown`は列のNULL率、`IS [NOT] NULL`は常に`is_unknown = 0`)。
`AND`、`OR`、`NOT`は、`src/estimator.rs`の`and3`、`or3`、`not3`が独立性を仮定しつつSQLの真理値表どおりに合成します。

```rust
pub fn and3(a: Selectivity3, b: Selectivity3) -> Selectivity3 {
    let is_true = (a.is_true * b.is_true).clamp(0.0, 1.0);
    let is_false = (a.is_false + b.is_false - a.is_false * b.is_false).clamp(0.0, 1.0);
    let is_unknown = (1.0 - is_true - is_false).max(0.0);
    Selectivity3 { is_true, is_false, is_unknown }
}

pub fn or3(a: Selectivity3, b: Selectivity3) -> Selectivity3 {
    let is_true = (a.is_true + b.is_true - a.is_true * b.is_true).clamp(0.0, 1.0);
    let is_false = (a.is_false * b.is_false).clamp(0.0, 1.0);
    let is_unknown = (1.0 - is_true - is_false).max(0.0);
    Selectivity3 { is_true, is_false, is_unknown }
}

pub fn not3(p: Selectivity3) -> Selectivity3 {
    Selectivity3 { is_true: p.is_false, is_false: p.is_true, is_unknown: p.is_unknown }
}
```

`AND`が`FALSE`になるのは「どちらか一方が`FALSE`」のとき(他方が`UNKNOWN`でもよい)なので、独立性を仮定した包除原理`P(a=F) + P(b=F) - P(a=F)P(b=F)`で`is_false`を求めます。
`AND`が`TRUE`になるのは「両方とも`TRUE`」のときに限られるので、`is_true`は積です。
`is_unknown`は残り(`1 - is_true - is_false`)として求めます。
`OR`はこの対称で、「どちらか一方が`TRUE`」で`is_true`が確定し(包除原理)、「両方とも`FALSE`」でだけ`is_false`が確定します(積)。
`NOT`は`TRUE`と`FALSE`を入れ替えるだけで、`UNKNOWN`はそのまま`UNKNOWN`にとどまります。

`predicate_selectivity`(`physical_plan`)は、式木を`Selectivity3`で再帰的にたどり、最後に`is_true`だけを取り出します。
`<>`(`NotEq`)も、独立した近似式を持たず`not3`(`=`の`Selectivity3`)として求めます。
第1巡目の`known_fraction`方式(旧`operand_non_null_fraction`と`estimate_not_selectivity`)は、この3確率方式に吸収する形で退役しました。

`AND`の独立性の仮定(2つの述語が互いに無関係に成り立つ)は、`status`と`amount`のように実際には相関する列の組み合わせでは崩れます。
この節ではまだ崩れないことにして、崩れる実例は後の節で`EXPLAIN ANALYZE`を使って確かめます。

### Join Cardinality

等値結合(第22章、第25章のJoinが前提とする範囲)の結果行数は、`src/estimator.rs`の`estimate_join_row_count`が標準的な式`|L| × |R| / max(NDV_l, NDV_r)`で見積もります。

```rust
pub fn estimate_join_row_count(left_rows: u64, right_rows: u64, left_ndv: u64, right_ndv: u64) -> u64 {
    let max_ndv = left_ndv.max(right_ndv).max(1);
    ((left_rows as u128 * right_rows as u128) / max_ndv as u128).min(u64::MAX as u128) as u64
}
```

結合キーの片方が主キー(NDVが行数と一致する一意な列)であれば、この式は「外側の各行に対して内側がちょうど1行だけ一致する」という主キーと外部キーの結合の典型的な状況にちょうど一致します。
`HashJoin`は`keys`(等値条件の対の並び、第22章)から結合キー列の実際のNDVを引けるため、`ANALYZE`済みならこの式をそのまま使います。
`NestedLoopJoin`は任意の条件(等号とは限らない)を持ち、結合キー列を機械的に特定できないため、「値はすべて一意」という最も楽観的な既定値(それぞれの出力行数そのもの)にフォールバックします。

`left_rows`と`right_rows`には、結合キー列が`NULL`の行を含めません。
`NULL`同士は等号で一致しない(`NULL = NULL`も`UNKNOWN`)ため、`physical_plan::apply_non_null_fraction`が、`ANALYZE`で観測した結合キー列のNULL率を使ってあらかじめ差し引いた非NULL行数を渡します。

### Aggregate後の行数

`GROUP BY`後の行数は、`src/estimator.rs`の`estimate_aggregate_row_count`が、`GROUP BY`に並ぶ各列のNDVの積で見積もります。

```rust
pub fn estimate_aggregate_row_count(group_ndvs: &[u64], input_rows: u64) -> u64 {
    if group_ndvs.is_empty() {
        // GROUP BYが無い集約(集約関数だけのSELECT)は、常にちょうど1行になる。
        return 1;
    }
    let mut product: u128 = 1;
    for &ndv in group_ndvs {
        product = product.saturating_mul(u128::from(ndv.max(1)));
        if product >= u128::from(input_rows) {
            return input_rows;
        }
    }
    product.min(u128::from(input_rows)) as u64
}
```

`dept`と`status`という2つの列で`GROUP BY`すれば、グループの上限は理屈のうえでは`NDV(dept) × NDV(status)`です。
ただし1グループは少なくとも1行を含むので、グループ数が入力行数を超えることはあり得ません。
積が`input_rows`を超えた時点で頭打ちにし(`u64`の掛け算オーバーフローを避けるため`u128`で計算します)、実際に組み合わせが少ない(相関がある)場合の過大評価を防ぎます。

### この章の推定はまだプラン選択に使わない

ここまでの推定式は、`EXPLAIN`と`EXPLAIN ANALYZE`の表示にだけ使います。
`physical_plan::optimize`(第22章、第25章)のアクセスパス選択とJoin方式選択のロジックはこの章では変更していません。
Index Nested Loop Joinを選ぶかHash Joinを選ぶかは、引き続き「索引があるかどうか」という構文的な性質だけで決まります。
この章が用意するのは、次章(第28章)がコストを見積もるための材料です。
コストという1つの数値へ複数の推定行数を合成し、実際にプラン選択へ反映させるのは第28章の仕事です。

## `EXPLAIN`と`EXPLAIN ANALYZE`

`EXPLAIN`(推定のみ)は、既存の`write_tree`が組み立てるラベルの末尾に、`rows=<推定値>`(先頭に半角スペース)を追記した形で表示します。

```console
minidb> EXPLAIN SELECT id FROM orders WHERE amount > 4000;
QUERY PLAN
----------
Projection(id) rows=150
  └─ Filter(amount > 4000) rows=150
    └─ SeqScan(orders) rows=1000
(3 rows)
```

`EXPLAIN ANALYZE`(PostgreSQLの`EXPLAIN ANALYZE`に相当)は、対象の文を実際に実行し、`actual=<実測値>`(こちらも先頭に半角スペース)を推定値と並べて表示します。

```console
minidb> EXPLAIN ANALYZE SELECT id FROM orders WHERE amount > 4000;
QUERY PLAN
----------
Projection(id) rows=150 actual=189
  └─ Filter(amount > 4000) rows=150 actual=189
    └─ SeqScan(orders) rows=1000 actual=1000
(3 rows)
```

実測行数は、`Box<dyn Executor>`(第19章)を`CountingExec`という薄いラッパーで包むことで集めます。
`CountingExec`は`src/physical_plan.rs`に定義します。

```rust
pub struct CountingExec<'a> {
    inner: Box<dyn Executor + 'a>,
    count: Rc<Cell<u64>>,
}

impl<'a> Executor for CountingExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.inner.output_schema()
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        let result = self.inner.next()?;
        if result.is_some() {
            self.count.set(self.count.get() + 1);
        }
        Ok(result)
    }
}
```

`build_query_executor`(第19章から`SELECT`の実行を担う関数)が`PhysicalPlan`の木をたどって`Executor`の木を組み立てるとき、各ノードを`CounterNode`(`PhysicalPlan::children()`と同じ形を持つ、カウンタだけの木)に対応する`CountingExec`で包みます。
カウンタを`Rc<Cell<u64>>`で持つのは、`Executor`の木を最後まで実行し終えて(=`Box<dyn Executor>`の所有権が尽きて)からでないと、`Database::execute_explain`側が各カウンタの値を読めないためです。
`&mut u64`のような通常の参照では、`Executor`の木を借用したまま結果を読み出すことになり、借用規則に反します。

`SELECT`は`Executor`の木を最後まで実行できるので、全ノードに`actual=`が付きます。
`INSERT`、`UPDATE`、`DELETE`は事情が違います。
`physical_plan`モジュール冒頭で説明されているとおり、この3つは`Executor`を経由しない一括処理(`run_insert`、`run_update`、`run_delete`)として実装されており、演算子ごとの内訳を計測する手段がありません。
そのためこの章では、`INSERT`、`UPDATE`、`DELETE`の`EXPLAIN ANALYZE`は**根のノード1行にだけ**`actual=`(実際に書き込まれた行数)を添え、`input`側(`Values`、`Scan`)のサブツリーは推定行数のみを表示します。

```console
minidb> EXPLAIN ANALYZE INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
QUERY PLAN
----------
Insert(users) rows=2 actual=2
  └─ Values(2 rows) rows=2
(2 rows)

minidb> EXPLAIN ANALYZE UPDATE users SET name = 'x' WHERE id = 1;
QUERY PLAN
----------
Update(users) rows=1000 actual=1
  └─ SeqScan(users) rows=1000
(2 rows)
```

もう1つ明記しておく必要があるのは、`EXPLAIN ANALYZE INSERT`/`UPDATE`/`DELETE`は**実際に書き込みを行う**という点です。
PostgreSQLの`EXPLAIN (ANALYZE, ...)`と異なり、この教材はトランザクション内で計測だけを取り消す機能を持たないため(トランザクションの導入はまだ先の章です)、`EXPLAIN ANALYZE INSERT`を実行すればテーブルの行は実際に増えます。

## 推定と実測のずれ: 相関する列

`AND`の推定式(独立性を仮定した積)は、2つの列が実際には相関しているとき、実測から大きくずれます。
`status`列と`amount`列に強い相関を持たせたテーブルで確かめます。

```console
minidb> CREATE TABLE orders (id BIGINT NOT NULL, status BIGINT NOT NULL, amount BIGINT NOT NULL);
CREATE TABLE
minidb> -- statusが1の行(200行)は必ずamountが5000以上、0の行(800行)は必ずamountが10
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN ANALYZE SELECT id FROM orders WHERE status = 1 AND amount > 100;
QUERY PLAN
----------
Projection(id) rows=40 actual=200
  └─ Filter(status = 1 AND amount > 100) rows=40 actual=200
    └─ SeqScan(orders) rows=1000 actual=1000
(3 rows)
```

`status = 1`はちょうど200行(全体の20%)に一致し、その200行は`amount`が必ず5000以上なので`amount > 100`も常に成り立ちます。
実測(`actual=200`)は、この「`status = 1`ならば必ず`amount > 100`」という関係をそのまま反映しています。
推定式は`estimate_and_selectivity(sel(status = 1), sel(amount > 100))`という積を計算しますが、これは`status`と`amount`が互いに無関係に決まるという仮定のもとでの見積もりです。
`sel(status = 1)`(0.2)、`sel(amount > 100)`(残りの800行は`amount = 10`で一致せず、200行は必ず一致するので、こちらも0.2)は、それぞれ単独では実データを正確に反映しています。
それでも実際には`status = 1`という条件が`amount > 100`をほぼ確定させてしまうため、2つの条件は独立ではなく、積による見積もり(`rows=40`)は実測(`actual=200`)の5分の1にとどまります。

この種のずれは、列同士の相関を持たない統計情報(列ごとのHistogramだけを持ち、列の組み合わせの分布は持たない)を使う限り避けられません。
複数列の相関を捉えるには、列の組み合わせごとのHistogram(Multi-column Statistics)のような、この章より進んだ統計情報が要ります。
`EXPLAIN ANALYZE`で`rows=`と`actual=`を並べて見せる仕組みは、こうした推定の限界を隠さず、どこでどれだけずれているかを利用者が確認できるようにするためのものです。

## 到達点

`ANALYZE`はテーブルの行数、列ごとのNULL数、NDV、Min/Max、MCV、等頻度Histogramを集め、`Backend::Disk`ではCatalogページへ永続化されます(永続化した統計は`validate_stats_metadata`が意味検証したうえで受理します)。
等値、範囲、`IS NULL`、`IS NOT NULL`、`AND`、`OR`、`NOT`、Join、`GROUP BY`後の行数という9種類の推定式が、この統計情報(または統計が無いときのデフォルト選択率)から行数を見積もり、`EXPLAIN`の`rows=`として表示されます。
`EXPLAIN ANALYZE`は実際に実行し、`actual=`で実測値と並べます。
相関する列の例が示すとおり、この推定はいつでも正しいわけではありません。

それでも、これで「選択的な結合」と「密な結合」を区別する材料は揃いました。
第28章は、この章が用意した推定行数を、I/OとCPUのコストへ変換し、複数のアクセスパスとJoin方式を数値で比較して選ぶコストベース最適化を扱います。

## 演習問題

### 必須課題

1. `estimate_equality_selectivity`は、Histogramのどのバケツにも収まらない値(観測範囲の外の値)に対して`DEFAULT_EQ_SEL`にフォールバックします。この挙動を、`ANALYZE`済みのテーブルに対して`WHERE id = <Max値より大きい定数>`を実行し、`EXPLAIN`の`rows=`が`DEFAULT_EQ_SEL`から計算した値になることを確認するテストで検証してください。
2. `estimate_aggregate_row_count`は`GROUP BY`の各列のNDVが独立だと仮定して積を取ります。`dept`と`dept_name`(`dept`ごとに1対1で決まる列)のように、実際には従属関係にある2列で`GROUP BY dept, dept_name`した場合、この仮定がどう破綻するかを実測し、`AND`の推定と同じ種類の限界であることを`EXPLAIN ANALYZE`を使って確認してください。
3. `StatsCollector::finish`は、非NULLの値をすべて`Vec<Value>`として保持してからソートし、Histogramを組み立てます(`distinct`とは別に、複製した値をもう1セット持ちます)。行数が数百万に達するテーブルでこのメモリ使用量がどう振る舞うか、`std::mem::size_of::<Value>()`を手がかりに見積もり、必要ならソート済みの一時ファイルへ退避するなど、メモリ使用量を減らす設計を検討してください(実装は必須ではありません)。
4. `IndexNestedLoopJoin`(第25章)の推定は、内側テーブルの列のNDVから「平均的な等値検索は何行返すか」(`1 / NDV`)を見積もっています(本文の`explain_text`の実装を参照)。この見積もりが、索引の値の分布が偏っている(一部の値に検索が集中する)場合にどれだけ実測とずれるかを、`ANALYZE`済みのテーブルと`EXPLAIN ANALYZE`を使って確認してください。

### 発展課題

1. この章のNDVは`HashSet`による厳密な計算です。HyperLogLogのような近似アルゴリズムを実際に実装し、行数を変えながら真のNDVとの誤差率を測定してください。行数が少ないうちは近似アルゴリズムの固定オーバーヘッド(レジスタ配列のメモリ)のほうが`HashSet`より不利になる分岐点がどこにあるかも確認してください。
2. `equality_selectivity_within_non_null`は、残余のHistogramのバケツ内でDistinct値が均等に散らばっていると仮定しています(`ndv_per_bucket`、バケツごとの実際のDistinct値数は記録していません)。`Bucket`にバケツごとのDistinct値数を追加で持たせる設計に変え、値がバケツ内で偏っている分布に対して推定精度がどう変わるかを実測してください。
3. 本文で触れた列同士の相関(Multi-column Statistics)を、簡易な形(2列の組み合わせごとのNDVだけを追加で持つなど)で実装し、「相関する列」の節で見た`rows=40 actual=200`というずれがどこまで縮まるかを確認してください。
