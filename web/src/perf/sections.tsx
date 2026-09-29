/** 性能页入口；图表、几何布局和各资源页面分别维护。 */
export { CpuSection } from "./sections/cpu";
export { MemberDetail } from "./sections/detail";
export { DiskSection } from "./sections/disk";
export { GpuSection } from "./sections/gpu";
export { MemSection } from "./sections/memory";
export { NetSection } from "./sections/network";
export { type PerfView, useSystemInfo } from "./sections/shared";
