import { draftMode } from "next/headers";
import { NextResponse } from "next/server";

export async function GET(request: Request) {
  const draft = await draftMode();
  draft.enable();
  return NextResponse.redirect(new URL("/preview", request.url));
}

export async function DELETE(request: Request) {
  const draft = await draftMode();
  draft.disable();
  return NextResponse.redirect(new URL("/preview", request.url));
}
