/**
 * 会话 token 在 localStorage 里的键。
 *
 * 单独一个模块：`session/useSession`（store）与 `workspace/termsocket`（裸 WS
 * 客户端）都要用它，而后者不该把整个 store 连同它的依赖（api client、控制面
 * WS、指标轮询）拖进来。两处各写一遍字符串则迟早漂移。
 */
export const SESSION_TOKEN_KEY = "strixmaid.session.token";
