declare module "*.css";
declare module "*.css?inline" {
  const text: string;
  export default text;
}
declare module "*.svg" {
  const url: string;
  export default url;
}
