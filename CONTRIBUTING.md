# 기여 규칙

## 커밋 제목

```
<종류>(<범위>): <요약>
```

- 종류: `feat` 새 기능, `fix` 버그 수정, `perf` 성능, `refactor` 동작이 같은 정리, `docs` 문서, `test` `build` `ci` `chore`
- 범위(선택): 모듈 이름(`midi` `layout` `schedule` `analyze` `player` `ui` `gui`) 또는 `build`
- 호환이 깨지면 종류 뒤에 `!`
- 요약은 사용자가 보는 변화를 쓴다. 본문에 이유와 방법을 적는다.

## 버전

버전은 `Cargo.toml`의 `version` 한 곳에 둔다. 출시할 때만 올린다.
