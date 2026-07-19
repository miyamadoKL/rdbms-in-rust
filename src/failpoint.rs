//! テスト専用のcrash point注入機構(第34章)。
//!
//! この章のCrash Testは、`crate::recovery::recover`の**途中**でプロセスが
//! 死んだ状況を再現する必要がある。第33章までのCrash Testは、`Database`を
//! `flush`せずに`drop`する(スコープの外に出す)だけで「直前の操作の後に
//! プロセスが死んだ」状況を模せた。それは、`COMMIT`やトランザクション境界の
//! *外側*からプロセスの終了を装えたからである。ところが`recover`は
//! `Storage::open`1回の呼び出しの*内部*で、Analysis・Redo・Undoを最後まで
//! 一気に完了させる。「Undoが半分終わったところでプロセスが死んだ」状況を
//! 外側から再現する手段が無い。
//!
//! この章はそのための、実プロセスを本当に停止させない注入機構を自作する。
//! `arm`で「この名前のfailpointが何回目に呼ばれたら失敗させるか」を予約し、
//! `recover`の内部が要所(Redoの1レコードごと、Undoの1トランザクションごと)で
//! [`hit`]を呼ぶ。回数が一致すると[`hit`]は`Err`を返し、`recover`はそれを
//! そのまま呼び出し元(`Storage::open`)へ伝播させる。
//!
//! # スレッドローカルにした理由
//!
//! `cargo test`は既定で複数のテストを別スレッドで並行実行する。armした
//! failpointをプロセス全体で共有する`static`にすると、あるテストがarmした
//! 状態を別のテストが誤って踏んでしまう。各`#[test]`関数は既定で専用の
//! スレッドを1本もらうため、`thread_local!`にしておけば、あるテストが
//! `arm`した内容は同じスレッド上のコードだけに見え、他のテストと干渉しない。
//!
//! # 一度だけ発火して自動でdisarmする
//!
//! [`hit`]は、指定回数目に到達した時点で1度だけ`Err`を返し、その後は
//! 自動的にarm状態を解除する。これにより、失敗を注入した後で同じテストが
//! (`arm`し直さずに)`Storage::open`をもう一度呼んでも、2回目の呼び出しは
//! 同じ場所で再度失敗せず最後まで完走できる。「1回目はN回目の直後で死ぬが、
//! 2回目に起動したときは同じ箇所で死なずに続行する」という、実際のクラッシュ
//! ・再起動の非決定性に対応する自然な挙動である。

use std::cell::RefCell;

use crate::error::{DbError, DbResult};

thread_local! {
    static ARMED: RefCell<Option<(&'static str, usize)>> = const { RefCell::new(None) };
}

/// `name`というfailpointを、`count`回目の[`hit`]呼び出しで発火するように
/// armする(`count == 1`なら次の1回目で即発火する)。
///
/// テストコードから呼ぶ。同じスレッド上で以前armした内容は上書きされる。
pub fn arm(name: &'static str, count: usize) {
    ARMED.with(|cell| *cell.borrow_mut() = Some((name, count.max(1))));
}

/// arm状態を解除する。テストの後始末、または「もう失敗させたくない」という
/// 明示的な意図を表すために呼ぶ。
pub fn disarm() {
    ARMED.with(|cell| *cell.borrow_mut() = None);
}

/// 内部の注入ポイントから呼ぶ。`name`が現在armされているfailpointと一致し、
/// 残り回数が0に達したら`Err`を返し、arm状態を自動的に解除する
/// (モジュール冒頭「一度だけ発火して自動でdisarmする」を参照)。
/// 一致しなければ(armされていない、または別の名前がarmされている)何もしない。
pub(crate) fn hit(name: &'static str) -> DbResult<()> {
    ARMED.with(|cell| {
        let mut slot = cell.borrow_mut();
        let fire = match slot.as_mut() {
            Some((armed_name, remaining)) if *armed_name == name => {
                *remaining -= 1;
                *remaining == 0
            }
            _ => false,
        };
        if fire {
            *slot = None;
            return Err(DbError::Io(std::io::Error::other(format!(
                "failpoint '{name}' が発火しました(第34章のCrash Test専用の注入)"
            ))));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_is_a_no_op_when_nothing_is_armed() {
        disarm();
        assert!(hit("anything").is_ok());
    }

    #[test]
    fn hit_fires_exactly_on_the_armed_count_then_disarms() {
        arm("step", 2);
        assert!(hit("step").is_ok(), "1回目はまだ発火しない");
        assert!(hit("step").is_err(), "2回目で発火する");
        assert!(hit("step").is_ok(), "発火後は自動でdisarmされ、以後は素通りする");
        disarm();
    }

    #[test]
    fn hit_ignores_a_different_name() {
        arm("this-one", 1);
        assert!(hit("that-one").is_ok(), "armした名前と違うfailpointは素通りする");
        disarm();
    }
}
